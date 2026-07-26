use deft_quick_js::console::{ConsoleBackend, Level};
use deft_quick_js::{Context, JsValue};

pub struct Console {

}

impl Console {
    pub fn new() -> Self {
        Self {}
    }
}

impl ConsoleBackend for Console {
    fn log(&self, level: Level, values: Vec<JsValue>) {
        println!("{}:{:?}", level, values);
    }

}

fn main() {
    let context = Context::builder().console(Console::new()).build().unwrap();
    context.create_module("native_module")
        .add_function("add", |a: i32, b: i32| {
            println!("add called : {} + {}", a, b);
            a + b
        })
        .build()
        .unwrap();
    context.eval_module(r#"
        import {add} from './native_module';
        add(4,5);
"#, "main.js").unwrap();

}