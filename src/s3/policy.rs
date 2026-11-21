//! Bucket policies: a JSON document checked before it is sent, so that a
//! typo is reported where it is rather than as the server's vague refusal.

use serde_json::Value;

/// The policy reformatted for reading, or the first problem found.
pub fn check(text: &str) -> Result<String, String> {
    let value: Value = serde_json::from_str(text).map_err(|error| {
        format!("JSON invalide (ligne {}, colonne {}) : {error}", error.line(), error.column())
    })?;
    let Value::Object(policy) = &value else {
        return Err("La politique doit être un objet JSON : { \"Version\": …, \"Statement\": […] }.".to_owned());
    };
    if let Some(version) = policy.get("Version")
        && !matches!(version.as_str(), Some("2012-10-17" | "2008-10-17"))
    {
        return Err("« Version » doit valoir \"2012-10-17\".".to_owned());
    }
    let statements = match policy.get("Statement") {
        Some(Value::Array(statements)) if !statements.is_empty() => statements.iter().collect::<Vec<_>>(),
        Some(Value::Object(_)) => vec![&policy["Statement"]],
        Some(_) => return Err("« Statement » doit contenir au moins une déclaration.".to_owned()),
        None => return Err("Il manque « Statement », la liste des déclarations.".to_owned()),
    };
    for (index, statement) in statements.iter().enumerate() {
        let name = match statement.get("Sid").and_then(Value::as_str) {
            Some(sid) => format!("Déclaration « {sid} »"),
            None => format!("Déclaration {}", index + 1),
        };
        let Value::Object(statement) = statement else {
            return Err(format!("{name} : une déclaration est un objet JSON."));
        };
        match statement.get("Effect").and_then(Value::as_str) {
            Some("Allow" | "Deny") => {}
            Some(other) => return Err(format!("{name} : « Effect » vaut \"Allow\" ou \"Deny\", pas \"{other}\".")),
            None => return Err(format!("{name} : il manque « Effect » (\"Allow\" ou \"Deny\").")),
        }
        let has = |one: &str, other: &str| statement.contains_key(one) || statement.contains_key(other);
        if !has("Principal", "NotPrincipal") {
            return Err(format!("{name} : il manque « Principal » (à qui la déclaration s'applique)."));
        }
        if !has("Action", "NotAction") {
            return Err(format!("{name} : il manque « Action »."));
        }
        if !has("Resource", "NotResource") {
            return Err(format!("{name} : il manque « Resource »."));
        }
    }
    Ok(reindent(text))
}

/// Reformats a policy for reading, as it was stored when it is not JSON.
pub fn pretty(text: &str) -> String {
    if serde_json::from_str::<Value>(text).is_ok() { reindent(text) } else { text.to_owned() }
}

/// Lays valid JSON out on lines, two spaces a level, keeping the order of
/// the keys (a parsed `Value` would sort them: "Version" would come last).
fn reindent(json: &str) -> String {
    let mut out = String::with_capacity(json.len() * 2);
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut chars = json.chars().peekable();
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        out.push_str(&"  ".repeat(depth));
    };
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '{' | '[' => {
                // Empty ones stay on their line.
                while chars.peek().is_some_and(|next| next.is_whitespace()) {
                    chars.next();
                }
                if matches!(chars.peek(), Some('}' | ']')) {
                    out.push(c);
                    out.extend(chars.next());
                } else {
                    depth += 1;
                    out.push(c);
                    newline(&mut out, depth);
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(c);
                newline(&mut out, depth);
            }
            ':' => out.push_str(": "),
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    out
}

/// A policy that lets anyone read every object of the bucket.
pub fn public_read(bucket: &str) -> String {
    let resource = serde_json::to_string(&format!("arn:aws:s3:::{bucket}/*")).unwrap_or_default();
    reindent(&format!(
        r#"{{"Version": "2012-10-17", "Statement": [{{"Sid": "LecturePublique", "Effect": "Allow", "Principal": "*", "Action": ["s3:GetObject"], "Resource": [{resource}]}}]}}"#
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_good_policy_passes_and_is_reformatted() {
        let policy = public_read("site");
        assert!(policy.contains("arn:aws:s3:::site/*"));
        assert_eq!(check(&policy), Ok(policy.clone()));
        let compact = r#"{"Version":"2012-10-17","Statement":{"Effect":"Deny","Principal":{"AWS":"*"},"Action":"s3:*","Resource":"*"}}"#;
        assert!(check(compact).unwrap().contains("\n"));
        assert_eq!(pretty("pas du json"), "pas du json");
    }

    #[test]
    fn layout_keeps_the_order_and_the_strings() {
        let compact = r#"{"Version":"2012-10-17","Statement":[{"Sid":"a, {b}: \"c\"","Action":[],"Condition":{}}]}"#;
        let laid = reindent(compact);
        assert_eq!(
            laid,
            "{\n  \"Version\": \"2012-10-17\",\n  \"Statement\": [\n    {\n      \"Sid\": \"a, {b}: \\\"c\\\"\",\n      \
             \"Action\": [],\n      \"Condition\": {}\n    }\n  ]\n}"
        );
        assert_eq!(reindent(&laid), laid, "laying out twice changes nothing");
        assert_eq!(
            serde_json::from_str::<Value>(&laid).unwrap(),
            serde_json::from_str::<Value>(compact).unwrap()
        );
        assert!(public_read("b").starts_with("{\n  \"Version\""));
    }

    #[test]
    fn mistakes_are_located() {
        assert!(check("{").unwrap_err().starts_with("JSON invalide (ligne 1"));
        assert!(check("[]").unwrap_err().contains("objet JSON"));
        assert!(check(r#"{"Version":"2012-10-17"}"#).unwrap_err().contains("Statement"));
        assert!(check(r#"{"Version":"2020","Statement":[]}"#).unwrap_err().contains("2012-10-17"));
        let no_effect = r#"{"Statement":[{"Sid":"A","Principal":"*","Action":"s3:GetObject","Resource":"*"}]}"#;
        assert_eq!(check(no_effect).unwrap_err(), "Déclaration « A » : il manque « Effect » (\"Allow\" ou \"Deny\").");
        let bad_effect = r#"{"Statement":[{"Effect":"Permit","Principal":"*","Action":"s3:GetObject","Resource":"*"}]}"#;
        assert!(check(bad_effect).unwrap_err().contains("Permit"));
        let no_resource = r#"{"Statement":[{"Effect":"Allow","Principal":"*","Action":"s3:GetObject"}]}"#;
        assert!(check(no_resource).unwrap_err().starts_with("Déclaration 1 : il manque « Resource »"));
    }
}
