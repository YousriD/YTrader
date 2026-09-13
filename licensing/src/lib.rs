//! Deliberately minimal today. The point isn't to enforce anything yet
//! — it's that `main()` already calls through this seam, so turning it
//! into a real license-key check (local signature verification, or a
//! phone-home activation call) later is a change to THIS file, not a
//! restructuring of the orchestrator.

pub enum LicenseStatus {
    /// No license configured — fine for personal use, not for
    /// distributing to someone else.
    Unlicensed,
    Valid { owner: String },
    Invalid { reason: String },
}

pub fn check() -> LicenseStatus {
    match std::env::var("TRADING_LICENSE_KEY") {
        Ok(_key) => {
            // TODO before selling: verify the key (e.g. an Ed25519
            // signature over an owner+expiry payload, checked purely
            // offline so a buyer doesn't need to phone home). For now,
            // presence of the var is treated as "trust it".
            LicenseStatus::Valid { owner: "unverified".to_string() }
        }
        Err(_) => LicenseStatus::Unlicensed,
    }
}
