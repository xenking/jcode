//! Live Jev Decisions check against a configured Jcode gateway.
//!
//! Ignored by default. Run with, for example:
//! JCODE_MEMORY_JEV_PROVIDER=jcode JCODE_BROWSER_JEV_PROVIDER=jcode \
//! JCODE_API_BASE=http://127.0.0.1:8787/v1 JCODE_API_KEY=... \
//! cargo test -p jcode-base --test jev_gateway_live -- --ignored --nocapture

use jcode_base::jev::JevClient;
use serde_json::json;

#[tokio::test]
#[ignore = "requires a live Jcode gateway with Jev configured"]
async fn live_gateway_answers_memory_and_browser_decisions() {
    let memory = JevClient::new().expect("memory Jev route");
    let questions = json!({
        "m0": {"type": "noul", "instructions": "Is this memory relevant to the task: User prefers Go 1.25 with Uber style",
               "criteria": {"true": "Useful for the task", "false": "Irrelevant"}},
        "m1": {"type": "noul", "instructions": "Is this memory relevant to the task: User's cat is named Miso",
               "criteria": {"true": "Useful for the task", "false": "Irrelevant"}}
    });
    let state = json!({"task": "Write a Go HTTP handler"});
    let answer = memory
        .evaluate(state, questions.as_object().unwrap().clone())
        .await
        .expect("memory decision");
    println!("memory: {answer}");
    let p = |id: &str| answer["answers"][id]["noul"].as_f64().unwrap();
    assert!(p("m0") > p("m1"));

    let browser = JevClient::for_browser().expect("browser Jev route");
    let questions = json!({"action": {"type": "choice", "instructions": "Choose the next browser action",
        "criteria": {"click": "Click the Submit button", "stop": "Return control to the user"}}});
    let answer = browser
        .evaluate(
            json!("The form is complete and a Submit button is visible."),
            questions.as_object().unwrap().clone(),
        )
        .await
        .expect("browser decision");
    println!("browser: {answer}");
    assert_eq!(answer["answers"]["action"]["type"], "choice");
}
