#!/usr/bin/env python3
"""yTrader MT5 bridge (P2-7): localhost HTTP sidecar for broker-mt5.

    yTrader (Rust) -> http://127.0.0.1:5001 (this file) -> MT5 terminal -> broker

RUN (Windows, MT5 terminal installed):
    pip install MetaTrader5
    python bridge.py            # terminal must be installed; login happens here

Your MT credentials live ONLY in this process's memory and your terminal.
They are never written to disk, never sent anywhere except your broker's
MT server, and yTrader never sees them. Do not paste them into chat.

STATUS: written against the official MetaTrader5 package API; NOT yet
run against a live terminal by the author (no terminal in that sandbox).
Validate with the 30-line script first, then point yTrader at this bridge.

Protocol (mirrors broker-mt5/src/lib.rs exactly — change both together):
    GET  /health                  -> {connected, login, server, trade_mode}
    GET  /account                 -> {balance, equity, currency, trade_allowed}
    GET  /positions?symbol=X      -> {positions: [{ticket, type, volume, price_open}]}
    GET  /price?symbol=X          -> {bid, ask}
    GET  /symbol_info?symbol=X    -> {contract_size, volume_min/max/step, trade_allowed}
    POST /order {symbol, side, volume_lots, comment, magic}
                                  -> {ticket, price, volume} | {error}
Side strings: "BUY" / "SELL". Position type ints follow MT5 convention
(POSITION_TYPE_BUY=0, POSITION_TYPE_SELL=1). trade_mode: DEMO/CONTEST/REAL.
"""

import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse, parse_qs

try:
    import MetaTrader5 as mt5
except ImportError:
    print("pip install MetaTrader5  (and install the MT5 terminal)", file=sys.stderr)
    sys.exit(2)

HOST, PORT = "127.0.0.1", 5001
YTRADER_MAGIC = 20260914


def trade_mode_str(info) -> str:
    return {0: "DEMO", 1: "CONTEST", 2: "REAL"}.get(info.trade_mode, "UNKNOWN")


class Handler(BaseHTTPRequestHandler):
    server_version = "ytrader-mt5-bridge/0.1"

    # -- helpers ---------------------------------------------------------
    def _send(self, obj, status=200):
        body = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _query(self):
        return parse_qs(urlparse(self.path).query)

    def log_message(self, *args):  # quieter stdout; yTrader logs instead
        pass

    # -- routes ----------------------------------------------------------
    def do_GET(self):
        path = urlparse(self.path).path
        q = self._query()
        sym = (q.get("symbol") or [None])[0]

        if path == "/health":
            info = mt5.account_info()
            if info is None:
                self._send({"connected": False, "trade_mode": "UNKNOWN"})
            else:
                self._send({
                    "connected": True,
                    "login": info.login,
                    "server": info.server,
                    "trade_mode": trade_mode_str(info),
                })
        elif path == "/account":
            info = mt5.account_info()
            if info is None:
                self._send({"error": "terminal not logged in"}, 503)
            else:
                self._send({
                    "balance": info.balance,
                    "equity": info.equity,
                    "currency": info.currency,
                    "trade_allowed": bool(info.trade_allowed),
                    "login": info.login,
                    "trade_mode": trade_mode_str(info),
                })
        elif path == "/positions":
            if not sym:
                self._send({"error": "missing ?symbol"}, 400)
                return
            raw = mt5.positions_get(symbol=sym) or ()
            self._send({"positions": [
                {"ticket": p.ticket, "type": p.type,
                 "volume": p.volume, "price_open": p.price_open}
                for p in raw
            ]})
        elif path == "/price":
            if not sym:
                self._send({"error": "missing ?symbol"}, 400)
                return
            tick = mt5.symbol_info_tick(sym)
            if tick is None:
                self._send({"error": f"no tick for {sym}"}, 404)
            else:
                self._send({"bid": tick.bid, "ask": tick.ask})
        elif path == "/symbol_info":
            if not sym:
                self._send({"error": "missing ?symbol"}, 400)
                return
            info = mt5.symbol_info(sym)
            if info is None:
                self._send({"error": f"unknown symbol {sym}"}, 404)
            else:
                # SYMBOL_TRADE_MODE: 0 DISABLED, 1 LONGONLY, 2 SHORTONLY,
                # 3 CLOSEONLY, 4 FULL.
                self._send({
                    "contract_size": info.trade_contract_size,
                    "volume_min": info.volume_min,
                    "volume_max": info.volume_max,
                    "volume_step": info.volume_step,
                    "trade_allowed": info.trade_mode not in (0, 3),
                })
        else:
            self._send({"error": "unknown route"}, 404)

    def do_POST(self):
        if urlparse(self.path).path != "/order":
            self._send({"error": "unknown route"}, 404)
            return
        try:
            length = int(self.headers.get("Content-Length", 0))
            req = json.loads(self.rfile.read(length) or b"{}")
        except Exception:
            self._send({"error": "invalid JSON body"}, 400)
            return
        try:
            result = place_market_order(
                req["symbol"], req["side"], float(req["volume_lots"]),
                str(req.get("comment", "ytrader")), int(req.get("magic", YTRADER_MAGIC)),
            )
        except (KeyError, ValueError, TypeError) as e:
            self._send({"error": f"bad field {e}"}, 400)
            return
        self._send(result, 200 if "error" not in result else 422)


def place_market_order(symbol, side, volume, comment, magic):
    """Market order via the official package. Returns fill-or-error dict."""
    if side not in ("BUY", "SELL"):
        return {"error": f"bad side {side!r}"}
    tick = mt5.symbol_info_tick(symbol)
    if tick is None:
        return {"error": f"no tick for {symbol}"}
    info = mt5.symbol_info(symbol)
    filling = mt5.ORDER_FILLING_IOC
    if info is not None and not (info.filling_mode & 2):
        filling = (mt5.ORDER_FILLING_FOK if info.filling_mode & 1
                   else mt5.ORDER_FILLING_RETURN)
    request = {
        "action": mt5.TRADE_ACTION_DEAL,
        "symbol": symbol,
        "volume": volume,
        "type": mt5.ORDER_TYPE_BUY if side == "BUY" else mt5.ORDER_TYPE_SELL,
        "price": tick.ask if side == "BUY" else tick.bid,
        "deviation": 20,
        "magic": magic,
        "comment": comment[:31],  # MT5 comment limit
        "type_time": mt5.ORDER_TIME_GTC,
        "type_filling": filling,
    }
    result = mt5.order_send(request)
    if result is None:
        return {"error": f"order_send failed: {mt5.last_error()}"}
    if result.retcode != mt5.TRADE_RETCODE_DONE:
        return {"error": f"retcode={result.retcode} {result.comment}"}
    return {"ticket": result.order, "price": result.price, "volume": result.volume}


def main():
    if not mt5.initialize():
        print(f"mt5.initialize() failed: {mt5.last_error()}", file=sys.stderr)
        print("Install + log into the MT5 terminal once first.", file=sys.stderr)
        sys.exit(1)
    info = mt5.account_info()
    if info is not None:
        print(f"bridge up: login={info.login} server={info.server} "
              f"mode={trade_mode_str(info)} balance={info.balance} {info.currency}")
    else:
        print("bridge up (terminal not logged in yet — log in via the terminal)")
    print(f"listening on http://{HOST}:{PORT}  (localhost only, Ctrl+C to stop)")
    try:
        ThreadingHTTPServer((HOST, PORT), Handler).serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        mt5.shutdown()


if __name__ == "__main__":
    main()
