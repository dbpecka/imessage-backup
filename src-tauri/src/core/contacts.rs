use std::collections::HashMap;
use std::path::PathBuf;

use rusqlite::{Connection, OpenFlags};

/// Phone numbers are matched on their last this-many digits when an exact
/// match fails, so a contact saved in national format (`07700 900123`)
/// still matches the international handle (`+447700900123`).
const PHONE_SUFFIX_LEN: usize = 9;

/// Normalized phone/email → display name.
#[derive(Debug, Default)]
pub struct ContactMap {
    exact: HashMap<String, String>,
    /// Last [`PHONE_SUFFIX_LEN`] digits → name, or `None` when different
    /// contacts share that suffix (ambiguous, so never used).
    suffix: HashMap<String, Option<String>>,
}

impl ContactMap {
    fn insert_phone(&mut self, phone: &str, name: &str) {
        let digits: String = phone.chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return;
        }
        for key in phone_lookup_keys(&digits) {
            self.exact.entry(key).or_insert_with(|| name.to_string());
        }
        if let Some(sfx) = phone_suffix(&digits) {
            self.suffix
                .entry(sfx)
                .and_modify(|existing| {
                    if existing.as_deref() != Some(name) {
                        *existing = None;
                    }
                })
                .or_insert_with(|| Some(name.to_string()));
        }
    }

    fn insert_email(&mut self, email: &str, name: &str) {
        let email_lower = email.trim().to_lowercase();
        if !email_lower.is_empty() {
            self.exact
                .entry(email_lower)
                .or_insert_with(|| name.to_string());
        }
    }

    /// Resolve a Messages `chat_identifier` / handle (phone or email) to a
    /// contact name.
    pub fn lookup(&self, identifier: &str) -> Option<String> {
        let id = identifier.strip_prefix("tel:").unwrap_or(identifier).trim();

        if id.contains('@') {
            return self.exact.get(&id.to_lowercase()).cloned();
        }

        // Only treat it as a phone number if it looks like one. Group chats
        // use opaque identifiers like `chat123456789`, whose digits must not
        // be matched against contacts' numbers.
        if id.is_empty()
            || !id
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, '+' | ' ' | '-' | '(' | ')' | '.'))
        {
            return None;
        }
        let digits: String = id.chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return None;
        }

        for key in phone_lookup_keys(&digits) {
            if let Some(name) = self.exact.get(&key) {
                return Some(name.clone());
            }
        }
        phone_suffix(&digits)
            .and_then(|sfx| self.suffix.get(&sfx))
            .and_then(|n| n.clone())
    }
}

fn phone_suffix(digits: &str) -> Option<String> {
    (digits.len() >= PHONE_SUFFIX_LEN)
        .then(|| digits[digits.len() - PHONE_SUFFIX_LEN..].to_string())
}

/// Fetch contacts from all local AddressBook sources.
///
/// Uses Full Disk Access (already required by the app) to read the
/// AddressBook SQLite databases directly. No separate Contacts TCC
/// permission is needed.
pub fn fetch_contact_map() -> ContactMap {
    let mut map = ContactMap::default();
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

fn load_phones(conn: &Connection, map: &mut ContactMap) {
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
        map.insert_phone(&phone, &name);
    }
}

fn load_emails(conn: &Connection, map: &mut ContactMap) {
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
        map.insert_email(&email, &name);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> ContactMap {
        let mut m = ContactMap::default();
        m.insert_phone("(555) 123-4567", "Ana US");
        m.insert_phone("07700 900123", "Ben UK");
        m.insert_email("Cara@Example.com", "Cara");
        m
    }

    #[test]
    fn matches_us_numbers_with_or_without_country_code() {
        let m = map();
        assert_eq!(m.lookup("+15551234567").as_deref(), Some("Ana US"));
        assert_eq!(m.lookup("tel:5551234567").as_deref(), Some("Ana US"));
    }

    #[test]
    fn matches_national_format_against_international_handle() {
        assert_eq!(map().lookup("+447700900123").as_deref(), Some("Ben UK"));
    }

    #[test]
    fn emails_are_case_insensitive() {
        assert_eq!(map().lookup("cara@example.COM").as_deref(), Some("Cara"));
    }

    #[test]
    fn group_chat_identifiers_never_match_numbers() {
        let mut m = map();
        m.insert_phone("123456789012", "Digits Person");
        assert_eq!(m.lookup("chat123456789012"), None);
    }

    #[test]
    fn ambiguous_suffixes_are_not_used() {
        let mut m = ContactMap::default();
        m.insert_phone("+44 7700 900123", "One");
        m.insert_phone("+61 7700 900123", "Two");
        // Exact matches still work…
        assert_eq!(m.lookup("+447700900123").as_deref(), Some("One"));
        // …but a third number sharing only the suffix resolves to nobody.
        assert_eq!(m.lookup("+3307700900123"), None);
    }
}
