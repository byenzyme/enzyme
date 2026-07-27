//! Shared end-to-end test harness for the `enzyme` CLI.
//!
//! Two pieces:
//!   * [`mock`] — wiremock-backed fake Enzyme API and fake OpenAI-compatible LLM.
//!   * [`fixture`] — a tempdir vault + isolated `ENZYME_HOME` + isolated API cache,
//!     with a subprocess runner for the real `enzyme` binary.
//!
//! Every test drives the CLI as a **subprocess** with an explicit environment, so
//! tests never mutate the test process's own env and can run in parallel.
//!
//! Typical shape:
//!
//! ```ignore
//! mod support;
//! use support::{mock::MockEnzymeApi, mock::MockLlm, fixture::VaultFixture};
//!
//! #[test]
//! fn my_test() {
//!     let api = MockEnzymeApi::start();
//!     let llm = MockLlm::start();
//!     llm.set_response(402, r#"{"error":"Insufficient credits"}"#);
//!
//!     let vault = VaultFixture::new();
//!     let out = vault
//!         .cli()
//!         .args(["init", "--quiet"])
//!         .env("OPENAI_API_KEY", "sk-test")
//!         .env("OPENAI_BASE_URL", llm.base_url())
//!         .enzyme_api(&api)
//!         .run();
//!
//!     assert!(out.stderr.contains("..."));
//!     assert_eq!(api.deduct_calls(), 0);
//! }
//! ```

#![allow(dead_code)]

pub mod epoch;
pub mod fixture;
pub mod mock;
