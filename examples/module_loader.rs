use std::io::Error;
use std::time::Instant;
use deft_quick_js::console::{ConsoleBackend, Level};
use deft_quick_js::{Context, JsValue};
use deft_quick_js::loader::JsModuleLoader;

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

struct MyModuleLoader;

impl JsModuleLoader for MyModuleLoader {
    fn load(&mut self, module_name: &str) -> Result<String, Error> {
        println!("Loading module {}", module_name);
        if module_name == "sys://fib" {
            return Ok(include_str!("./fib.js").to_string());
        }
        Ok(include_str!("./js-module.js").to_string())
    }

    fn resolve_module_path(&mut self, _base_module_name: &str, module_name: &str) -> Option<String> {
        println!("Resolving module {}", module_name);
        if module_name == "fib" {
            Some("sys://fib".to_string())
        } else {
            None
        }
    }
}

pub fn main() {
    let start_time = Instant::now();
    let context = Context::builder().console(Console::new())
        .module_loader(MyModuleLoader)
        .build().unwrap();

    let value = context.eval_module("import {fib} from 'fib';import {add} from 'static://js-module'; add(1, 2); __console_write('log', import.meta.url);", "test/main.js").unwrap();
    println!("init time: {}ms", start_time.elapsed().as_millis());
    println!("result {:?}", value);
}
