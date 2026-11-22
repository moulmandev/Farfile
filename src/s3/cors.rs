//! CORS rules: the JSON document as the AWS CLI prints it, the form of the
//! editor, and the conversions to and from the SDK.

use anyhow::{Result, bail};
use aws_sdk_s3::types::{CorsConfiguration, CorsRule};
use serde::{Deserialize, Serialize};

use super::lifecycle::json_error;

/// S3 refuses more rules than this.
pub const MAX_RULES: usize = 100;
pub const METHODS: [&str; 5] = ["GET", "PUT", "POST", "DELETE", "HEAD"];

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorsDoc {
    #[serde(rename = "CORSRules", default)]
    pub rules: Vec<CorsRuleDoc>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct CorsRuleDoc {
    #[serde(rename = "ID", default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_headers: Vec<String>,
    pub allowed_methods: Vec<String>,
    pub allowed_origins: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_seconds: Option<i32>,
}

impl CorsDoc {
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    pub fn from_json(text: &str) -> Result<Self> {
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(text).map_err(|error| json_error(&error))
    }

    pub fn validate(&self) -> Result<()> {
        if self.rules.len() > MAX_RULES {
            bail!(tr!("S3 accepte au plus {max} règles CORS.", max = MAX_RULES));
        }
        for (index, rule) in self.rules.iter().enumerate() {
            if let Some(problem) = rule.problem() {
                bail!(tr!("{name} : {problem}", name = rule.label(index), problem));
            }
        }
        Ok(())
    }

    /// The configuration to send. Empty means "remove the configuration".
    pub fn to_sdk(&self) -> Result<Option<CorsConfiguration>> {
        self.validate()?;
        if self.rules.is_empty() {
            return Ok(None);
        }
        let rules = self
            .rules
            .iter()
            .map(|rule| {
                let list = |items: &[String]| (!items.is_empty()).then(|| items.to_vec());
                CorsRule::builder()
                    .set_id(rule.id.clone().filter(|id| !id.is_empty()))
                    .set_allowed_headers(list(&rule.allowed_headers))
                    .set_allowed_methods(Some(rule.allowed_methods.clone()))
                    .set_allowed_origins(Some(rule.allowed_origins.clone()))
                    .set_expose_headers(list(&rule.expose_headers))
                    .set_max_age_seconds(rule.max_age_seconds)
                    .build()
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(CorsConfiguration::builder().set_cors_rules(Some(rules)).build()?))
    }

    pub fn from_sdk(rules: &[CorsRule]) -> Self {
        Self {
            rules: rules
                .iter()
                .map(|rule| CorsRuleDoc {
                    id: rule.id().map(str::to_owned),
                    allowed_headers: rule.allowed_headers().to_vec(),
                    allowed_methods: rule.allowed_methods().to_vec(),
                    allowed_origins: rule.allowed_origins().to_vec(),
                    expose_headers: rule.expose_headers().to_vec(),
                    max_age_seconds: rule.max_age_seconds(),
                })
                .collect(),
        }
    }
}

/// One rule as the editor shows it: lists are typed one item per line (or
/// separated by commas), numbers stay text while typed.
#[derive(Clone, Debug, PartialEq)]
pub struct CorsForm {
    pub id: String,
    pub origins: String,
    /// Ticked methods, in the order of [`METHODS`].
    pub methods: [bool; 5],
    pub allowed_headers: String,
    pub expose_headers: String,
    pub max_age: String,
}

impl Default for CorsForm {
    /// What most web pages reading a bucket need.
    fn default() -> Self {
        Self {
            id: String::new(),
            origins: "*".to_owned(),
            methods: [true, false, false, false, true],
            allowed_headers: "*".to_owned(),
            expose_headers: "ETag".to_owned(),
            max_age: "3000".to_owned(),
        }
    }
}

impl CorsRuleDoc {
    /// How the rule is called in messages: its name, or its rank.
    pub fn label(&self, index: usize) -> String {
        match self.id.as_deref().filter(|id| !id.is_empty()) {
            Some(id) => tr!("Règle « {id} »", id),
            None => tr!("Règle {n}", n = index + 1),
        }
    }

    /// What S3 would refuse in this rule, said without naming it.
    pub fn problem(&self) -> Option<String> {
        if self.allowed_origins.iter().all(|origin| origin.trim().is_empty()) {
            return Some(tr!("indiquez au moins une origine (par exemple https://exemple.fr ou *).").to_owned());
        }
        if let Some(origin) = self.allowed_origins.iter().find(|origin| origin.matches('*').count() > 1) {
            return Some(tr!("l'origine « {origin} » contient plus d'un « * ».", origin));
        }
        if self.allowed_methods.is_empty() {
            return Some(tr!("choisissez au moins une méthode.").to_owned());
        }
        if let Some(method) = self.allowed_methods.iter().find(|method| !METHODS.contains(&method.as_str())) {
            return Some(tr!("méthode « {method} » inconnue (GET, PUT, POST, DELETE ou HEAD).", method));
        }
        if self.max_age_seconds.is_some_and(|age| age < 0) {
            return Some(tr!("la durée de cache ne peut pas être négative.").to_owned());
        }
        None
    }
}

impl CorsForm {
    pub fn from_doc(rule: &CorsRuleDoc) -> Self {
        Self {
            id: rule.id.clone().unwrap_or_default(),
            origins: rule.allowed_origins.join("\n"),
            methods: METHODS.map(|method| rule.allowed_methods.iter().any(|m| m.eq_ignore_ascii_case(method))),
            allowed_headers: rule.allowed_headers.join("\n"),
            expose_headers: rule.expose_headers.join("\n"),
            max_age: rule.max_age_seconds.map(|age| age.to_string()).unwrap_or_default(),
        }
    }

    pub fn to_doc(&self) -> Result<CorsRuleDoc, String> {
        let max_age = match self.max_age.trim() {
            "" => None,
            text => match text.parse::<i32>() {
                Ok(age) if age >= 0 => Some(age),
                _ => return Err(tr!("Durée du cache : indiquez un nombre de secondes.").to_owned()),
            },
        };
        let id = self.id.trim();
        Ok(CorsRuleDoc {
            id: (!id.is_empty()).then(|| id.to_owned()),
            allowed_headers: items(&self.allowed_headers),
            allowed_methods: METHODS
                .iter()
                .zip(self.methods)
                .filter(|(_, ticked)| *ticked)
                .map(|(method, _)| (*method).to_owned())
                .collect(),
            allowed_origins: items(&self.origins),
            expose_headers: items(&self.expose_headers),
            max_age_seconds: max_age,
        })
    }
}

/// The whole configuration from the forms, or the first problem found.
pub fn doc_from_forms(forms: &[CorsForm]) -> Result<CorsDoc, String> {
    let rules = forms
        .iter()
        .enumerate()
        .map(|(index, form)| {
            form.to_doc()
                .map_err(|error| tr!("{name} : {problem}", name = tr!("Règle {n}", n = index + 1), problem = error))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let doc = CorsDoc { rules };
    doc.validate().map_err(|error| format!("{error:#}"))?;
    Ok(doc)
}

/// Splits a list typed one item per line or separated by commas.
fn items(text: &str) -> Vec<String> {
    text.split(['\n', ','])
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_form_builds_a_rule() {
        let form = CorsForm {
            origins: "https://a.example\nhttps://b.example, https://c.example".to_owned(),
            methods: [true, true, false, false, false],
            max_age: "600".to_owned(),
            ..CorsForm::default()
        };
        let rule = form.to_doc().unwrap();
        assert_eq!(rule.allowed_origins, ["https://a.example", "https://b.example", "https://c.example"]);
        assert_eq!(rule.allowed_methods, ["GET", "PUT"]);
        assert_eq!(rule.allowed_headers, ["*"]);
        assert_eq!(rule.expose_headers, ["ETag"]);
        assert_eq!(rule.max_age_seconds, Some(600));

        let doc = CorsDoc { rules: vec![rule] };
        let sdk = doc.to_sdk().unwrap().unwrap();
        assert_eq!(CorsDoc::from_sdk(sdk.cors_rules()), doc);
        assert_eq!(CorsForm::from_doc(&doc.rules[0]).methods, form.methods);
        assert_eq!(CorsDoc::from_json(&doc.to_json()).unwrap(), doc);
        assert!(doc.to_json().contains("\"CORSRules\""));
    }

    #[test]
    fn mistakes_are_explained() {
        let no_origin = CorsForm {
            origins: " \n ".to_owned(),
            ..CorsForm::default()
        };
        assert!(doc_from_forms(&[no_origin]).unwrap_err().contains("origine"));
        let no_method = CorsForm {
            methods: [false; 5],
            ..CorsForm::default()
        };
        assert!(doc_from_forms(&[no_method]).unwrap_err().contains("méthode"));
        let bad_age = CorsForm {
            max_age: "une heure".to_owned(),
            ..CorsForm::default()
        };
        assert!(doc_from_forms(&[bad_age]).unwrap_err().contains("secondes"));
        let doc = CorsDoc::from_json(r#"{"CORSRules": [{"AllowedMethods": ["PATCH"], "AllowedOrigins": ["*"]}]}"#).unwrap();
        assert!(doc.validate().unwrap_err().to_string().contains("PATCH"));
        assert!(CorsDoc::from_json(r#"{"Rules": []}"#).is_err(), "unknown fields are refused");
        assert_eq!(CorsDoc::default().to_sdk().unwrap(), None);
    }
}
