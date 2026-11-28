use jcode_applet_types::compile_express;
use serde_json::{Value, json};

fn main() {
    let cases = [
        (
            "GreetingButton",
            "English",
            "Show a greeting button for Ada",
            ["Say hello", "greet", "Ada"],
        ),
        (
            "GreetingButton",
            "Russian",
            "Показать кнопку приветствия для Ады",
            ["Поздороваться", "greet", "Ада"],
        ),
        (
            "ReviewChange",
            "English",
            "Show the change for review",
            ["Review patch", "Update the login screen", "change42"],
        ),
        (
            "ReviewChange",
            "Russian",
            "Показать изменение для проверки",
            ["Проверить исправление", "Обновить экран входа", "change42"],
        ),
    ];
    let rows: Vec<Value> = cases
        .into_iter()
        .map(|(name, language, intent, args)| {
            let literals = args.map(|arg| serde_json::to_string(arg).unwrap());
            let source = format!(
                "root = {name}({},{},{})",
                literals[0], literals[1], literals[2]
            );
            let view = compile_express(&source).expect("fixture must compile");
            json!({
                "macro": name,
                "language": language,
                "express": {"action":"mount", "intent": intent, "express": source},
                "native": {"action":"mount", "intent": intent, "view": view},
            })
        })
        .collect();
    println!("{}", serde_json::to_string(&rows).unwrap());
}
