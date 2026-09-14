@echo off
REM yTrader native MT5 demo launcher. No Python or MetaTrader5 package needed.
setlocal
cd /d "%~dp0"
echo === yTrader native MT5 demo ===
echo.
echo Compile bridge-mt5\YTraderNativeBridge.mq5 in MetaEditor, attach it to a
echo DEMO chart, and allow 127.0.0.1 in MT5's Expert Advisor network list.
echo Leave the EA's EnableOrders=false for the first health check.
echo.
where cargo >nul 2>&1
if errorlevel 1 (
  echo [ERROR] Rust cargo was not found on PATH.
  pause
  exit /b 1
)
powershell -NoProfile -Command "try { Invoke-RestMethod http://127.0.0.1:5001/health -TimeoutSec 2 | Out-Null; exit 0 } catch { exit 1 }" >nul 2>&1
if errorlevel 1 (
  start "yTrader MT5 Native Sidecar" cmd /k cargo run -p mt5-sidecar
  echo [..] sidecar starting...
) else (
  echo [OK] Reusing the existing local sidecar.
)
powershell -NoProfile -Command "$t=0; while ($t -lt 30) { try { $r=Invoke-RestMethod http://127.0.0.1:5001/health -TimeoutSec 2; if ($r.connected -and $r.trade_mode -eq 'DEMO') { exit 0 } } catch {}; Start-Sleep 1; $t++ }; exit 1" >nul 2>&1
if errorlevel 1 (
  echo [ERROR] EA did not publish a connected DEMO account within 30 seconds.
  echo Check the MT5 Experts tab for its WebRequest permission message.
  pause
  exit /b 1
)
echo [OK] MT5 DEMO account connected. Starting yTrader...
cargo run -p orchestrator -- demo-mt5.toml
echo.
echo Run finished. Close the "yTrader MT5 Native Sidecar" window when done.
pause
