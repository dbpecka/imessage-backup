#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::path::PathBuf;

use rusqlite::{Connection, OpenFlags};

/// Fetch contacts from all local AddressBook sources and return a map of
/// normalized phone/email → display name.
///
/// Uses Full Disk Access (already required by the app) to read the
/// AddressBook SQLite databases directly. No separate Contacts TCC
/// permission is needed.
pub fn fetch_contact_map() -> HashMap<String, String> {
    let mut map = HashMap::new();
    for path in addressbook_db_paths() {
        if !path.exists() {
            continue;
        }
        let Ok(conn) = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) else {
            continue;
        };
        load_phones(&conn, &mut map);
        load_emails(&conn, &mut map);
    }
    map
}

fn addressbook_db_paths() -> Vec<PathBuf> {
    let home = match std::env::var("HOME") {
        Ok(h) => PathBuf::from(h),
        Err(_) => return vec![],
    };
    let root = home.join("Library/Application Support/AddressBook");
    let mut paths = vec![root.join("AddressBook-v22.abcddb")];

    if let Ok(entries) = std::fs::read_dir(root.join("Sources")) {
        for entry in entries.flatten() {
            paths.push(entry.path().join("AddressBook-v22.abcddb"));
        }
    }
    paths
}

fn load_phones(conn: &Connection, map: &mut HashMap<String, String>) {
    let mut stmt = match conn.prepare(
        "SELECT r.ZFIRSTNAME, r.ZLASTNAME, r.ZNICKNAME, r.ZORGANIZATION, p.ZFULLNUMBER
         FROM ZABCDRECORD r
         INNER JOIN ZABCDPHONENUMBER p ON p.ZOWNER = r.Z_PK
         WHERE p.ZFULLNUMBER IS NOT NULL AND p.ZFULLNUMBER != ''",
    ) {
        Ok(s) => s,
        Err(_) => return,
    };

    let Ok(rows) = stmt.query_map([], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, String>(4)?,
        ))
    }) else {
        return;
    };

    for row in rows.flatten() {
        let (first, last, nick, org, phone) = row;
        let Some(name) = build_name(
            first.as_deref(),
            last.as_deref(),
            nick.as_deref(),
            org.as_deref(),
        ) else {
            continue;
        };
        let digits: String = phone.chars().filter(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            for key in phone_lookup_keys(&digits) {
                map.entry(key).or_insert_with(|| name.clone());
            }
        }
    }
}

fn load_emails(conn: &Connection, map: &mut HashMap<String, String>) {
    let mut stmt = match conn.prepare(
        "SELECT r.ZFIRSTNAME, r.ZLASTNAME, r.ZNICKNAME, r.ZORGANIZATION, e.ZADDRESS
         FROM ZABCDRECORD r
         INNER JOIN ZABCDEMAILADDRESS e ON e.ZOWNER = r.Z_PK
         WHERE e.ZADDRESS IS NOT NULL AND e.ZADDRESS != ''",
    ) {
        Ok(s) => s,
        Err(_) => return,
    };

    let Ok(rows) = stmt.query_map([], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, String>(4)?,
        ))
    }) else {
        return;
    };

    for row in rows.flatten() {
        let (first, last, nick, org, email) = row;
        let Some(name) = build_name(
            first.as_deref(),
            last.as_deref(),
            nick.as_deref(),
            org.as_deref(),
        ) else {
            continue;
        };
        let email_lower = email.to_lowercase();
        if !email_lower.is_empty() {
            map.entry(email_lower).or_insert_with(|| name.clone());
        }
    }
}

fn build_name(
    first: Option<&str>,
    last: Option<&str>,
    nick: Option<&str>,
    org: Option<&str>,
) -> Option<String> {
    let first = first.map(str::trim).filter(|s| !s.is_empty());
    let last = last.map(str::trim).filter(|s| !s.is_empty());
    match (first, last) {
        (Some(f), Some(l)) => return Some(format!("{f} {l}")),
        (Some(f), None) => return Some(f.to_string()),
        (None, Some(l)) => return Some(l.to_string()),
        (None, None) => {}
    }
    if let Some(n) = nick.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(n.to_string());
    }
    org.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn phone_lookup_keys(digits: &str) -> Vec<String> {
    let mut keys = vec![digits.to_string()];
    // 11-digit US/CA numbers starting with 1: also index the 10-digit form.
    if digits.len() == 11 && digits.starts_with('1') {
        keys.push(digits[1..].to_string());
    } else if digits.len() == 10 {
        // Also index with leading country code 1.
        keys.push(format!("1{digits}"));
    }
    keys
}

/// Resolve a Messages `chat_identifier` (phone or email) to a contact name.
pub fn lookup_contact_name(identifier: &str, map: &HashMap<String, String>) -> Option<String> {
    let id = identifier.strip_prefix("tel:").unwrap_or(identifier);

    if id.contains('@') {
        return map.get(&id.to_lowercase()).cloned();
    }

    let digits: String = id.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }

    for key in phone_lookup_keys(&digits) {
        if let Some(name) = map.get(&key) {
            return Some(name.clone());
        }
    }
    None
}
