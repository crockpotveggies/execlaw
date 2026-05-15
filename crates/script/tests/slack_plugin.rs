use execlaw_script::{ScriptEngine, ScriptPlugin};

#[test]
fn slack_main_rhai_parses() {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); p.pop();
    p.push("plugins/slack/main.rhai");
    let src = std::fs::read_to_string(&p).unwrap();
    let factory = ScriptEngine::new();
    ScriptPlugin::from_source("slack", &src, &factory)
        .expect("plugins/slack/main.rhai must parse");
}
