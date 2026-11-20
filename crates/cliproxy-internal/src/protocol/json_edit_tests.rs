//! Property tables for the JSON operations used by request transformations.
use crate::protocol::json;
use serde_json::{Value, json};

#[test]
fn json_edits_agree_with_an_independent_value_model() {
    // serde supplies an independent semantic model. No proxy output is used as an expectation.
    for length in [0, 1, 2, 8, 32] {
        for target in [
            Value::Null,
            json!(true),
            json!(-7),
            json!("\"é\\\n"),
            json!([1, 2]),
            json!({"k":1}),
        ] {
            let mut model = json!({"items": (0..length).map(|n| json!({"value":n})).collect::<Vec<_>>(), "other":{"raw":"unchanged"}});
            let input = serde_json::to_vec_pretty(&model).unwrap();
            for index in 0..length {
                let path = format!("items.{index}.value");
                let mut expected = model.clone();
                expected["items"][index]["value"] = target.clone();
                let output = json::try_set_raw(&input, &path, target.to_string()).unwrap();
                assert_eq!(serde_json::from_slice::<Value>(&output).unwrap(), expected);
                assert_eq!(json::get(&output, &path).raw(), target.to_string().as_bytes());
                let old_other = json::get(&input, "other").raw().to_vec();
                assert_eq!(json::get(&output, "other").raw(), old_other);
                let deleted = json::try_delete(&output, &path).unwrap();
                expected["items"][index].as_object_mut().unwrap().remove("value");
                assert_eq!(serde_json::from_slice::<Value>(&deleted).unwrap(), expected);
            }
            model["new"] = target.clone();
            let output = json::try_set_raw(&input, "new", target.to_string()).unwrap();
            assert_eq!(serde_json::from_slice::<Value>(&output).unwrap(), model);
            assert_eq!(json::try_delete(&input, "absent.path").unwrap(), input);
        }
    }
}
