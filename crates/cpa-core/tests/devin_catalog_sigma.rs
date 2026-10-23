//! The Devin catalog keys IDs with Go `strings.ToLower` (`ΟΣ` becomes `devin/οσ`, where
//! Rust's `to_lowercase` would give a final sigma), and admission compares with Go
//! `EqualFold` (`ClientSupportsModel`), where σ and ς are equal: a request for `devin/ος`
//! reaches the credential. Its own test binary: it replaces the process-wide Devin
//! catalog, which the library's unit tests read on parallel threads.
use std::path::Path;
use std::sync::Arc;

use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_core::registry::dynamic::Registry;

#[test]
fn devin_admission_folds_final_sigma_like_go() {
    assert_eq!(
        cpa_core::registry::devin::global().load(r#"{"devin":[{"id":"ΟΣ"}]}"#.as_bytes(), "test"),
        Ok(true)
    );
    let metadata = serde_json::json!({"type": "devin"}).as_object().cloned().unwrap();
    let c = Arc::new(Credential::from_file(Path::new("/a"), &Path::new("/a").join("d.json"), metadata).unwrap());
    let r = Registry::build(&Config::default(), std::slice::from_ref(&c));
    assert!(r.ids().any(|id| id == "devin/οσ"), "{:?}", r.ids().collect::<Vec<_>>());
    assert!(r.client_supports("d.json", "devin/ος"));
    assert_eq!(r.client_model("d.json", "devin/ος").unwrap().id, "devin/οσ");
}
