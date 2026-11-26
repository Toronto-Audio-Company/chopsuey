//! Issues chopsuey licence keys.
//!
//! ```text
//! cargo run -p licence_keygen -- new-keypair --out <private-key-file> [--force]
//! cargo run -p licence_keygen -- issue --key <private-key-file> --name "Ada Lovelace" --email ada@example.com [--id N] [--date YYYY-MM-DD]
//! cargo run -p licence_keygen -- public-key --key <private-key-file>
//! cargo run -p licence_keygen -- verify --key <private-key-file> <licence-key>
//! ```
//!
//! The private key file holds the 32-byte Ed25519 seed as hex. Anyone who has
//! it can mint licences, so production keys live outside the repository.

use licence_format::{issue, public_key_from_seed, verify, Licence};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

/// The product this tool issues keys for.
const PRODUCT: &str = "chopsuey";

const USAGE: &str = "\
usage:
  keygen new-keypair --out <private-key-file> [--force]
      Makes a new signing keypair, writes the private key to the file and
      prints the public key to paste into src/licence.rs.
  keygen issue --key <private-key-file> --name <name> --email <email> [--id <n>] [--date YYYY-MM-DD]
      Prints a signed licence key. The key id is random unless given; the
      date defaults to today (UTC).
  keygen public-key --key <private-key-file>
      Prints the public key belonging to a private key file.
  keygen verify --key <private-key-file> <licence-key>
      Checks a licence key against that keypair and prints what it says.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(out) => {
            println!("{out}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    NewKeypair { out: PathBuf, force: bool },
    Issue { key: PathBuf, name: String, email: String, id: Option<u32>, date: Option<u32> },
    PublicKey { key: PathBuf },
    Verify { key: PathBuf, licence: String },
}

fn run(args: &[String]) -> Result<String, String> {
    match parse_args(args)? {
        Command::NewKeypair { out, force } => {
            let mut seed = [0u8; 32];
            getrandom::getrandom(&mut seed).map_err(|e| format!("no system randomness: {e}"))?;
            write_private_key(&out, &seed, force)?;
            Ok(format!(
                "Wrote the private key to {}. Keep it secret and out of the repository.\n\n\
                 Public key for src/licence.rs:\n\n{}",
                out.display(),
                public_key_rust(&public_key_from_seed(&seed))
            ))
        }
        Command::Issue { key, name, email, id, date } => {
            let seed = read_private_key(&key)?;
            let id = match id {
                Some(id) => id,
                None => {
                    let mut b = [0u8; 4];
                    getrandom::getrandom(&mut b).map_err(|e| format!("no system randomness: {e}"))?;
                    u32::from_be_bytes(b)
                }
            };
            let date = date.unwrap_or_else(today);
            let licence = Licence::new(PRODUCT, &name, &email, id, date);
            issue(&licence, &seed).map_err(|e| e.to_string())
        }
        Command::PublicKey { key } => Ok(public_key_rust(&public_key_from_seed(&read_private_key(&key)?))),
        Command::Verify { key, licence } => {
            let public = public_key_from_seed(&read_private_key(&key)?);
            let l = verify(&licence, PRODUCT, &public).map_err(|e| e.to_string())?;
            let (y, m, d) = civil_from_days(l.issued_days as i64);
            Ok(format!(
                "valid {} licence (format v{})\n  name:   {}\n  email:  {}\n  key id: {}\n  issued: {y:04}-{m:02}-{d:02}",
                l.product, l.version, l.name, l.email, l.key_id
            ))
        }
    }
}

fn parse_args(args: &[String]) -> Result<Command, String> {
    let (cmd, rest) = args.split_first().ok_or("no command given")?;
    let mut flags: Vec<(String, Option<String>)> = Vec::new();
    let mut positional = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == "--force" {
            flags.push((a.clone(), None));
        } else if let Some(name) = a.strip_prefix("--") {
            let value = it.next().ok_or_else(|| format!("--{name} needs a value"))?;
            flags.push((format!("--{name}"), Some(value.clone())));
        } else {
            positional.push(a.clone());
        }
    }
    let mut take = |name: &str| -> Option<String> {
        let i = flags.iter().position(|(f, _)| f == name)?;
        flags.remove(i).1
    };
    let need = |v: Option<String>, name: &str| v.ok_or_else(|| format!("{name} is required"));

    let command = match cmd.as_str() {
        "new-keypair" => {
            let out = need(take("--out"), "--out")?;
            let force = flags.iter().any(|(f, _)| f == "--force");
            flags.retain(|(f, _)| f != "--force");
            Command::NewKeypair { out: out.into(), force }
        }
        "issue" => {
            let key = need(take("--key"), "--key")?.into();
            let name = need(take("--name"), "--name")?;
            let email = need(take("--email"), "--email")?;
            let id = take("--id")
                .map(|s| s.parse::<u32>().map_err(|_| format!("--id must be a whole number up to {}", u32::MAX)))
                .transpose()?;
            let date = take("--date").map(|s| parse_date(&s)).transpose()?;
            Command::Issue { key, name, email, id, date }
        }
        "public-key" => Command::PublicKey { key: need(take("--key"), "--key")?.into() },
        "verify" => {
            let key = need(take("--key"), "--key")?.into();
            if positional.is_empty() {
                return Err("verify needs the licence key to check".into());
            }
            let licence = positional.join(" ");
            positional.clear();
            Command::Verify { key, licence }
        }
        other => return Err(format!("unknown command {other:?}")),
    };
    if let Some((f, _)) = flags.first() {
        return Err(format!("unexpected option {f}"));
    }
    if let Some(p) = positional.first() {
        return Err(format!("unexpected argument {p:?}"));
    }
    Ok(command)
}

/// A Rust constant to paste over `PUBLIC_KEY` in src/licence.rs.
fn public_key_rust(key: &[u8; 32]) -> String {
    let mut s = String::from("const PUBLIC_KEY: [u8; 32] = [\n");
    for row in key.chunks(8) {
        let hex: Vec<String> = row.iter().map(|b| format!("0x{b:02x}")).collect();
        s.push_str(&format!("    {},\n", hex.join(", ")));
    }
    s.push_str("];");
    s
}

fn write_private_key(path: &Path, seed: &[u8; 32], force: bool) -> Result<(), String> {
    use std::io::Write;
    let contents = format!(
        "# chopsuey licence signing key (Ed25519 private key seed, hex).\n\
         # Anyone with this file can issue licences. Never commit it.\n{}\n",
        to_hex(seed)
    );
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true);
    if force {
        opts.create(true).truncate(true);
    } else {
        opts.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            format!("{} already exists (pass --force to replace it)", path.display())
        } else {
            format!("can't write {}: {e}", path.display())
        }
    })?;
    f.write_all(contents.as_bytes()).map_err(|e| format!("can't write {}: {e}", path.display()))
}

fn read_private_key(path: &Path) -> Result<[u8; 32], String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
    let hex: String = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    from_hex(&hex)
        .and_then(|b| <[u8; 32]>::try_from(b).ok())
        .ok_or_else(|| format!("{} doesn't hold a 64-digit hex private key", path.display()))
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

fn today() -> u32 {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    (secs / 86_400) as u32
}

/// `YYYY-MM-DD` to days since 1970-01-01.
fn parse_date(s: &str) -> Result<u32, String> {
    let bad = || format!("--date must look like 2026-11-25, not {s:?}");
    let parts: Vec<&str> = s.split('-').collect();
    let [y, m, d] = parts.as_slice() else { return Err(bad()) };
    let (y, m, d) = (
        y.parse::<i64>().map_err(|_| bad())?,
        m.parse::<u32>().map_err(|_| bad())?,
        d.parse::<u32>().map_err(|_| bad())?,
    );
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(bad());
    }
    let days = days_from_civil(y, m, d);
    if civil_from_days(days) != (y, m, d) {
        return Err(bad()); // e.g. 2026-02-30
    }
    u32::try_from(days).map_err(|_| bad())
}

// Howard Hinnant's civil-date algorithms (proleptic Gregorian, UTC).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|a| a.to_string()).collect()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("licence-keygen-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn dates_round_trip() {
        assert_eq!(parse_date("1970-01-01"), Ok(0));
        assert_eq!(parse_date("2026-11-25"), Ok(20_782));
        assert_eq!(civil_from_days(20_782), (2026, 11, 25));
        assert!(parse_date("2026-02-30").is_err());
        assert!(parse_date("2026-13-01").is_err());
        assert!(parse_date("yesterday").is_err());
        assert!(parse_date("1969-12-31").is_err());
    }

    #[test]
    fn parses_commands() {
        assert_eq!(
            parse_args(&args(&["issue", "--key", "k.txt", "--name", "Ada", "--email", "a@b.c", "--id", "7"])),
            Ok(Command::Issue { key: "k.txt".into(), name: "Ada".into(), email: "a@b.c".into(), id: Some(7), date: None })
        );
        assert_eq!(
            parse_args(&args(&["new-keypair", "--force", "--out", "k.txt"])),
            Ok(Command::NewKeypair { out: "k.txt".into(), force: true })
        );
        assert!(parse_args(&args(&["issue", "--key", "k.txt", "--name", "Ada"])).is_err());
        assert!(parse_args(&args(&["issue", "--key", "k", "--name", "A", "--email", "e", "--bogus", "x"])).is_err());
        assert!(parse_args(&args(&["new-keypair"])).is_err());
        assert!(parse_args(&args(&["frobnicate"])).is_err());
        assert!(parse_args(&[]).is_err());
    }

    #[test]
    fn keypair_issue_verify_end_to_end() {
        let dir = scratch("e2e");
        let key_file = dir.join("private.txt");
        let key_arg = key_file.to_string_lossy().to_string();

        let out = run(&args(&["new-keypair", "--out", &key_arg])).unwrap();
        assert!(out.contains("const PUBLIC_KEY: [u8; 32]"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&key_file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Never clobbers an existing key by accident.
        assert!(run(&args(&["new-keypair", "--out", &key_arg])).unwrap_err().contains("already exists"));

        let licence = run(&args(&[
            "issue", "--key", &key_arg, "--name", "Ada Lovelace", "--email", "ada@example.com", "--id", "99",
            "--date", "2026-11-25",
        ]))
        .unwrap();
        assert!(licence.starts_with("CHOPSUEY-"));

        let report = run(&args(&["verify", "--key", &key_arg, &licence])).unwrap();
        assert!(report.contains("Ada Lovelace") && report.contains("key id: 99") && report.contains("2026-11-25"));

        // The printed public key is the one that verifies.
        let seed = read_private_key(&key_file).unwrap();
        let printed = run(&args(&["public-key", "--key", &key_arg])).unwrap();
        assert_eq!(printed, public_key_rust(&public_key_from_seed(&seed)));
        assert!(licence_format::verify(&licence, "chopsuey", &public_key_from_seed(&seed)).is_ok());

        // A different keypair rejects it.
        let other = dir.join("other.txt");
        let other_arg = other.to_string_lossy().to_string();
        run(&args(&["new-keypair", "--out", &other_arg])).unwrap();
        assert!(run(&args(&["verify", "--key", &other_arg, &licence])).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_bad_private_key_files() {
        let dir = scratch("bad");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("k.txt");
        std::fs::write(&f, "# comment\nnot hex\n").unwrap();
        assert!(read_private_key(&f).is_err());
        std::fs::write(&f, "abcd\n").unwrap();
        assert!(read_private_key(&f).is_err());
        assert!(read_private_key(&dir.join("missing.txt")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
