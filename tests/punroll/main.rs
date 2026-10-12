#[derive(Clone, Copy)]
pub struct RustcVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

const fn parse_part(input: &mut &[u8]) -> u16 {
    let mut val = 0;
    let mut idx = 0;
    while idx < input.len() {
        let v = input[idx];
        match v {
            b'0'..=b'9' => {
                val = val * 10 + (v - b'0') as u16;
            }
            b'.' => {
                idx += 1;
                break;
            }
            _ => panic!("invalid character in version"),
        }
        idx += 1;
    }
    *input = input.split_at(idx).1;
    val
}

#[no_mangle]
pub fn parse_rust_version(version: &str) -> RustcVersion {
    let mut bytes = version.as_bytes();
    let major = parse_part(&mut bytes);
    let minor = parse_part(&mut bytes);
    let patch = parse_part(&mut bytes);
    assert!(bytes.is_empty());
    RustcVersion {
        major,
        minor,
        patch,
    }
}

fn main() {
    let v = parse_rust_version("1.2.3");
    assert_eq!((v.major, v.minor, v.patch), (1, 2, 3));
    println!("ok");
}
