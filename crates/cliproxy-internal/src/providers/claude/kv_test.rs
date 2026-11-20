//! An in-memory Home KV behind the fake Home, for the Home-mode cache tests.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// An in-memory KV behind the fake Home, honouring NX and XX.
pub(crate) fn kv_home(values: Arc<Mutex<HashMap<String, String>>>) -> impl Fn(&[String]) -> crate::home::fake::Reply {
    use crate::home::fake::{bulk, raw};
    move |args| {
        let mut values = values.lock().unwrap();
        match args[0].to_ascii_lowercase().as_str() {
            "get" => values.get(&args[1]).map_or_else(|| raw("$-1\r\n"), bulk),
            "set" => {
                let exists = values.contains_key(&args[1]);
                let flag = |f: &str| args.iter().any(|a| a == f);
                if (flag("NX") && exists) || (flag("XX") && !exists) {
                    return raw("$-1\r\n");
                }
                values.insert(args[1].clone(), args[2].clone());
                raw("+OK\r\n")
            }
            // Home's `CAS <key> <0|1> <expected> <new> [PX ms]`.
            "cas" => {
                let current = values.get(&args[1]);
                let matches = match args[2].as_str() {
                    "0" => current.is_none(),
                    _ => current == Some(&args[3]),
                };
                if !matches {
                    return raw(":0\r\n");
                }
                values.insert(args[1].clone(), args[4].clone());
                raw(":1\r\n")
            }
            "del" => {
                let deleted = args[1..].iter().filter(|k| values.remove(*k).is_some()).count();
                raw(&format!(":{deleted}\r\n"))
            }
            "expire" => raw(if values.contains_key(&args[1]) {
                ":1\r\n"
            } else {
                ":0\r\n"
            }),
            _ => raw("+PONG\r\n"),
        }
    }
}
