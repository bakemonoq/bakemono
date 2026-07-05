use serde::Serialize;
use serde_json::Value;

use crate::error::{Error, Result};

// explicit key sort instead of relying on serde_json's Map ordering: any crate in the
// dependency tree enabling `preserve_order` would silently change the signed bytes
pub fn to_canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let value = serde_json::to_value(value).map_err(|e| Error::Build(e.to_string()))?;
    let mut out = Vec::new();
    write_value(&value, &mut out);
    Ok(out)
}

fn write_value(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            out.push(b'{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_scalar(&Value::String((*key).clone()), out);
                out.push(b':');
                write_value(&map[key.as_str()], out);
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_value(item, out);
            }
            out.push(b']');
        }
        scalar => write_scalar(scalar, out),
    }
}

fn write_scalar(value: &Value, out: &mut Vec<u8>) {
    serde_json::to_writer(&mut *out, value).expect("writing a scalar to a Vec cannot fail");
}
