//! Lifecycle rules: the JSON document as the AWS CLI prints it, the small
//! form of the editor, and the conversions to and from the SDK.
//!
//! The editor covers the common rules (expire after N days, delete
//! non-current versions, abort incomplete uploads, with a prefix). Anything
//! else a rule holds (transitions, tags, dates…) is kept as it is when the
//! rule is edited, and can be changed in the JSON view.

use anyhow::{Context, Result, bail};
use aws_sdk_s3::primitives::{DateTime, DateTimeFormat};
use aws_sdk_s3::types::{
    AbortIncompleteMultipartUpload, BucketLifecycleConfiguration, ExpirationStatus, LifecycleExpiration,
    LifecycleRule, LifecycleRuleAndOperator, LifecycleRuleFilter, NoncurrentVersionExpiration,
    NoncurrentVersionTransition, Tag, Transition, TransitionStorageClass,
};
use serde::{Deserialize, Serialize};

/// S3 refuses more rules than this in one configuration.
pub const MAX_RULES: usize = 1000;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct LifecycleDoc {
    #[serde(default)]
    pub rules: Vec<RuleDoc>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct RuleDoc {
    #[serde(rename = "ID", default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The old way of filtering, still returned by some servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<FilterDoc>,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiration: Option<ExpirationDoc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transitions: Vec<TransitionDoc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noncurrent_version_expiration: Option<NoncurrentExpirationDoc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub noncurrent_version_transitions: Vec<NoncurrentTransitionDoc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abort_incomplete_multipart_upload: Option<AbortDoc>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    #[default]
    Enabled,
    Disabled,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct FilterDoc {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<TagDoc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_size_greater_than: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_size_less_than: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub and: Option<AndDoc>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct AndDoc {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<TagDoc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_size_greater_than: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_size_less_than: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct TagDoc {
    pub key: String,
    pub value: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct ExpirationDoc {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days: Option<i32>,
    /// ISO 8601, e.g. `2027-01-01T00:00:00Z`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expired_object_delete_marker: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct TransitionDoc {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct NoncurrentExpirationDoc {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noncurrent_days: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newer_noncurrent_versions: Option<i32>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct NoncurrentTransitionDoc {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noncurrent_days: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newer_noncurrent_versions: Option<i32>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct AbortDoc {
    pub days_after_initiation: i32,
}

impl LifecycleDoc {
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// Reads the document typed in the JSON view.
    pub fn from_json(text: &str) -> Result<Self> {
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        let doc: Self = serde_json::from_str(text).map_err(|error| json_error(&error))?;
        Ok(doc)
    }

    /// Checks what S3 would refuse, with a message that says where.
    pub fn validate(&self) -> Result<()> {
        if self.rules.len() > MAX_RULES {
            bail!("S3 accepte au plus {MAX_RULES} règles.");
        }
        let mut ids = std::collections::HashSet::new();
        for (index, rule) in self.rules.iter().enumerate() {
            let name = rule.label(index);
            if let Some(id) = &rule.id {
                if id.chars().count() > 255 {
                    bail!("{name} : le nom ne peut pas dépasser 255 caractères.");
                }
                if !id.is_empty() && !ids.insert(id.as_str()) {
                    bail!("Deux règles portent le nom « {id} ».");
                }
            }
            rule.validate().with_context(|| name.clone())?;
        }
        Ok(())
    }

    /// The configuration to send. Empty means "remove the configuration".
    pub fn to_sdk(&self) -> Result<Option<BucketLifecycleConfiguration>> {
        self.validate()?;
        if self.rules.is_empty() {
            return Ok(None);
        }
        let rules = self.rules.iter().map(RuleDoc::to_sdk).collect::<Result<Vec<_>>>()?;
        Ok(Some(BucketLifecycleConfiguration::builder().set_rules(Some(rules)).build()?))
    }

    pub fn from_sdk(rules: &[LifecycleRule]) -> Self {
        Self {
            rules: rules.iter().map(RuleDoc::from_sdk).collect(),
        }
    }
}

impl RuleDoc {
    /// How the rule is called in messages: its name, or its rank.
    pub fn label(&self, index: usize) -> String {
        match self.id.as_deref().filter(|id| !id.is_empty()) {
            Some(id) => format!("Règle « {id} »"),
            None => format!("Règle {}", index + 1),
        }
    }

    fn validate(&self) -> Result<()> {
        let positive = |days: Option<i32>, what: &str| -> Result<()> {
            match days {
                Some(days) if days < 1 => bail!("{what} : le nombre de jours doit être d'au moins 1."),
                _ => Ok(()),
            }
        };
        let has_action = self.expiration.is_some()
            || !self.transitions.is_empty()
            || self.noncurrent_version_expiration.is_some()
            || !self.noncurrent_version_transitions.is_empty()
            || self.abort_incomplete_multipart_upload.is_some();
        if !has_action {
            bail!("choisissez au moins une action.");
        }
        if let Some(expiration) = &self.expiration {
            positive(expiration.days, "expiration")?;
            let set = [
                expiration.days.is_some(),
                expiration.date.is_some(),
                expiration.expired_object_delete_marker.is_some(),
            ];
            match set.iter().filter(|set| **set).count() {
                0 => bail!("l'expiration doit donner Days, Date ou ExpiredObjectDeleteMarker."),
                1 => {}
                _ => bail!("l'expiration ne peut donner qu'un seul de Days, Date et ExpiredObjectDeleteMarker."),
            }
            if let Some(date) = &expiration.date {
                parse_date(date)?;
            }
        }
        for transition in &self.transitions {
            positive(transition.days, "transition")?;
            if transition.storage_class.as_deref().unwrap_or("").is_empty() {
                bail!("une transition doit donner sa classe de stockage (StorageClass).");
            }
            if let Some(date) = &transition.date {
                parse_date(date)?;
            }
        }
        if let Some(noncurrent) = &self.noncurrent_version_expiration {
            positive(noncurrent.noncurrent_days, "versions non actuelles")?;
            if noncurrent.noncurrent_days.is_none() {
                bail!("la suppression des versions non actuelles doit donner NoncurrentDays.");
            }
        }
        for transition in &self.noncurrent_version_transitions {
            positive(transition.noncurrent_days, "transition des versions non actuelles")?;
        }
        if let Some(abort) = &self.abort_incomplete_multipart_upload {
            positive(Some(abort.days_after_initiation), "envois incomplets")?;
            let tagged = self.filter.as_ref().is_some_and(|filter| {
                filter.tag.is_some() || filter.and.as_ref().is_some_and(|and| !and.tags.is_empty())
            });
            if tagged {
                bail!("l'interruption des envois incomplets ne peut pas filtrer par étiquette.");
            }
        }
        Ok(())
    }

    /// The top-level prefix is deprecated, but a rule written with it is
    /// sent back as it was.
    #[allow(deprecated)]
    fn to_sdk(&self) -> Result<LifecycleRule> {
        let expiration = self
            .expiration
            .as_ref()
            .map(|expiration| -> Result<LifecycleExpiration> {
                Ok(LifecycleExpiration::builder()
                    .set_days(expiration.days)
                    .set_date(expiration.date.as_deref().map(parse_date).transpose()?)
                    .set_expired_object_delete_marker(expiration.expired_object_delete_marker)
                    .build())
            })
            .transpose()?;
        let transitions = self
            .transitions
            .iter()
            .map(|transition| -> Result<Transition> {
                Ok(Transition::builder()
                    .set_days(transition.days)
                    .set_date(transition.date.as_deref().map(parse_date).transpose()?)
                    .set_storage_class(transition.storage_class.as_deref().map(TransitionStorageClass::from))
                    .build())
            })
            .collect::<Result<Vec<_>>>()?;
        let noncurrent_transitions = self
            .noncurrent_version_transitions
            .iter()
            .map(|transition| {
                NoncurrentVersionTransition::builder()
                    .set_noncurrent_days(transition.noncurrent_days)
                    .set_storage_class(transition.storage_class.as_deref().map(TransitionStorageClass::from))
                    .set_newer_noncurrent_versions(transition.newer_noncurrent_versions)
                    .build()
            })
            .collect::<Vec<_>>();

        // A rule without any filter applies to the whole bucket: S3 wants
        // an empty filter rather than none.
        let filter = match (&self.filter, &self.prefix) {
            (Some(filter), _) => Some(filter.to_sdk()?),
            (None, Some(_)) => None,
            (None, None) => Some(LifecycleRuleFilter::builder().prefix("").build()),
        };
        let status = match self.status {
            Status::Enabled => ExpirationStatus::Enabled,
            Status::Disabled => ExpirationStatus::Disabled,
        };
        Ok(LifecycleRule::builder()
            .set_id(self.id.clone().filter(|id| !id.is_empty()))
            .set_prefix(self.prefix.clone())
            .set_filter(filter)
            .status(status)
            .set_expiration(expiration)
            .set_transitions((!transitions.is_empty()).then_some(transitions))
            .set_noncurrent_version_expiration(self.noncurrent_version_expiration.as_ref().map(|noncurrent| {
                NoncurrentVersionExpiration::builder()
                    .set_noncurrent_days(noncurrent.noncurrent_days)
                    .set_newer_noncurrent_versions(noncurrent.newer_noncurrent_versions)
                    .build()
            }))
            .set_noncurrent_version_transitions((!noncurrent_transitions.is_empty()).then_some(noncurrent_transitions))
            .set_abort_incomplete_multipart_upload(self.abort_incomplete_multipart_upload.as_ref().map(|abort| {
                AbortIncompleteMultipartUpload::builder()
                    .days_after_initiation(abort.days_after_initiation)
                    .build()
            }))
            .build()?)
    }

    #[allow(deprecated)]
    fn from_sdk(rule: &LifecycleRule) -> Self {
        let date = |date: Option<&DateTime>| date.and_then(|date| date.fmt(DateTimeFormat::DateTime).ok());
        let filter = rule.filter().map(FilterDoc::from_sdk).filter(|filter| *filter != FilterDoc::default());
        Self {
            id: rule.id().map(str::to_owned),
            prefix: rule.prefix().map(str::to_owned),
            // An empty filter and no filter mean the same: the whole bucket.
            filter,
            status: if *rule.status() == ExpirationStatus::Disabled {
                Status::Disabled
            } else {
                Status::Enabled
            },
            expiration: rule.expiration().map(|expiration| ExpirationDoc {
                days: expiration.days(),
                date: date(expiration.date()),
                expired_object_delete_marker: expiration.expired_object_delete_marker(),
            }),
            transitions: rule
                .transitions()
                .iter()
                .map(|transition| TransitionDoc {
                    days: transition.days(),
                    date: date(transition.date()),
                    storage_class: transition.storage_class().map(|class| class.as_str().to_owned()),
                })
                .collect(),
            noncurrent_version_expiration: rule.noncurrent_version_expiration().map(|noncurrent| {
                NoncurrentExpirationDoc {
                    noncurrent_days: noncurrent.noncurrent_days(),
                    newer_noncurrent_versions: noncurrent.newer_noncurrent_versions(),
                }
            }),
            noncurrent_version_transitions: rule
                .noncurrent_version_transitions()
                .iter()
                .map(|transition| NoncurrentTransitionDoc {
                    noncurrent_days: transition.noncurrent_days(),
                    storage_class: transition.storage_class().map(|class| class.as_str().to_owned()),
                    newer_noncurrent_versions: transition.newer_noncurrent_versions(),
                })
                .collect(),
            abort_incomplete_multipart_upload: rule
                .abort_incomplete_multipart_upload()
                .and_then(|abort| abort.days_after_initiation())
                .map(|days| AbortDoc {
                    days_after_initiation: days,
                }),
        }
    }

    /// What the simple editor cannot show, for a note next to the rule.
    pub fn advanced(&self) -> Vec<&'static str> {
        let mut parts = Vec::new();
        if self.filter.as_ref().is_some_and(|filter| !filter.is_simple()) {
            parts.push("filtre avancé");
        }
        if !self.transitions.is_empty() || !self.noncurrent_version_transitions.is_empty() {
            parts.push("transitions");
        }
        if let Some(expiration) = &self.expiration {
            if expiration.date.is_some() {
                parts.push("expiration à date fixe");
            }
            if expiration.expired_object_delete_marker.is_some() {
                parts.push("marqueurs de suppression orphelins");
            }
        }
        if self
            .noncurrent_version_expiration
            .as_ref()
            .is_some_and(|noncurrent| noncurrent.newer_noncurrent_versions.is_some())
        {
            parts.push("versions récentes conservées");
        }
        parts
    }

    /// One line for the list of rules.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(days) = self.expiration.as_ref().and_then(|expiration| expiration.days) {
            parts.push(format!("expire après {}", days_text(days)));
        }
        if let Some(days) = self.noncurrent_version_expiration.as_ref().and_then(|n| n.noncurrent_days) {
            parts.push(format!("anciennes versions supprimées après {}", days_text(days)));
        }
        if let Some(abort) = &self.abort_incomplete_multipart_upload {
            parts.push(format!("envois incomplets interrompus après {}", days_text(abort.days_after_initiation)));
        }
        parts.extend(self.advanced().into_iter().map(str::to_owned));
        if parts.is_empty() {
            "aucune action".to_owned()
        } else {
            parts.join(" · ")
        }
    }
}

impl FilterDoc {
    /// Only a prefix, which the form can edit.
    fn is_simple(&self) -> bool {
        self.tag.is_none()
            && self.object_size_greater_than.is_none()
            && self.object_size_less_than.is_none()
            && self.and.is_none()
    }

    fn to_sdk(&self) -> Result<LifecycleRuleFilter> {
        let tag = |tag: &TagDoc| -> Result<Tag> { Ok(Tag::builder().key(&tag.key).value(&tag.value).build()?) };
        let and = self
            .and
            .as_ref()
            .map(|and| -> Result<LifecycleRuleAndOperator> {
                let tags = and.tags.iter().map(tag).collect::<Result<Vec<_>>>()?;
                Ok(LifecycleRuleAndOperator::builder()
                    .set_prefix(and.prefix.clone())
                    .set_tags((!tags.is_empty()).then_some(tags))
                    .set_object_size_greater_than(and.object_size_greater_than)
                    .set_object_size_less_than(and.object_size_less_than)
                    .build())
            })
            .transpose()?;
        Ok(LifecycleRuleFilter::builder()
            .set_prefix(self.prefix.clone())
            .set_tag(self.tag.as_ref().map(tag).transpose()?)
            .set_object_size_greater_than(self.object_size_greater_than)
            .set_object_size_less_than(self.object_size_less_than)
            .set_and(and)
            .build())
    }

    fn from_sdk(filter: &LifecycleRuleFilter) -> Self {
        let tag = |tag: &Tag| TagDoc {
            key: tag.key().to_owned(),
            value: tag.value().to_owned(),
        };
        Self {
            prefix: filter.prefix().map(str::to_owned).filter(|prefix| !prefix.is_empty()),
            tag: filter.tag().map(tag),
            object_size_greater_than: filter.object_size_greater_than(),
            object_size_less_than: filter.object_size_less_than(),
            and: filter.and().map(|and| AndDoc {
                prefix: and.prefix().map(str::to_owned),
                tags: and.tags().iter().map(tag).collect(),
                object_size_greater_than: and.object_size_greater_than(),
                object_size_less_than: and.object_size_less_than(),
            }),
        }
    }
}

fn parse_date(text: &str) -> Result<DateTime> {
    DateTime::from_str(text, DateTimeFormat::DateTime)
        .with_context(|| format!("date « {text} » illisible : attendu 2027-01-31T00:00:00Z"))
}

pub fn days_text(days: i32) -> String {
    if days > 1 { format!("{days} jours") } else { format!("{days} jour") }
}

/// serde's message, located in the text.
pub fn json_error(error: &serde_json::Error) -> anyhow::Error {
    anyhow::anyhow!("JSON invalide (ligne {}, colonne {}) : {error}", error.line(), error.column())
}

/// One rule as the simple editor shows it. Numbers stay text while typed.
#[derive(Clone, Debug, PartialEq)]
pub struct RuleForm {
    pub id: String,
    pub prefix: String,
    pub enabled: bool,
    pub expire: bool,
    pub expire_days: String,
    pub noncurrent: bool,
    pub noncurrent_days: String,
    pub abort: bool,
    pub abort_days: String,
    /// The rule as it was loaded: what the form does not show is kept.
    pub base: RuleDoc,
}

impl Default for RuleForm {
    fn default() -> Self {
        Self::from_doc(&RuleDoc::default())
    }
}

impl RuleForm {
    pub fn from_doc(rule: &RuleDoc) -> Self {
        let expire_days = rule.expiration.as_ref().and_then(|expiration| expiration.days);
        let noncurrent_days = rule
            .noncurrent_version_expiration
            .as_ref()
            .and_then(|noncurrent| noncurrent.noncurrent_days);
        let abort_days = rule
            .abort_incomplete_multipart_upload
            .as_ref()
            .map(|abort| abort.days_after_initiation);
        let text = |days: Option<i32>, default: &str| days.map_or_else(|| default.to_owned(), |days| days.to_string());
        Self {
            id: rule.id.clone().unwrap_or_default(),
            prefix: rule
                .filter
                .as_ref()
                .and_then(|filter| filter.prefix.clone())
                .or_else(|| rule.prefix.clone())
                .unwrap_or_default(),
            enabled: rule.status == Status::Enabled,
            expire: expire_days.is_some(),
            expire_days: text(expire_days, "30"),
            noncurrent: noncurrent_days.is_some(),
            noncurrent_days: text(noncurrent_days, "30"),
            abort: abort_days.is_some(),
            abort_days: text(abort_days, "7"),
            base: rule.clone(),
        }
    }

    /// The prefix can only be edited when it is the whole filter.
    pub fn prefix_editable(&self) -> bool {
        self.base.filter.as_ref().is_none_or(FilterDoc::is_simple)
    }

    /// The rule to save, or what is wrong with the form.
    pub fn to_doc(&self) -> Result<RuleDoc, String> {
        let days = |text: &str, what: &str| -> Result<i32, String> {
            match text.trim().parse::<i32>() {
                Ok(days) if days >= 1 => Ok(days),
                _ => Err(format!("{what} : indiquez un nombre de jours d'au moins 1.")),
            }
        };
        let mut rule = self.base.clone();
        let id = self.id.trim();
        rule.id = (!id.is_empty()).then(|| id.to_owned());
        rule.status = if self.enabled { Status::Enabled } else { Status::Disabled };

        if self.prefix_editable() {
            // The current way of filtering replaces the old one.
            rule.prefix = None;
            let prefix = self.prefix.trim_start_matches('/');
            rule.filter = (!prefix.is_empty()).then(|| FilterDoc {
                prefix: Some(prefix.to_owned()),
                ..FilterDoc::default()
            });
        }

        let had_days = self.base.expiration.as_ref().is_some_and(|expiration| expiration.days.is_some());
        if self.expire {
            rule.expiration = Some(ExpirationDoc {
                days: Some(days(&self.expire_days, "Expiration")?),
                ..ExpirationDoc::default()
            });
        } else if had_days {
            rule.expiration = None;
        }

        if self.noncurrent {
            let newer = self
                .base
                .noncurrent_version_expiration
                .as_ref()
                .and_then(|noncurrent| noncurrent.newer_noncurrent_versions);
            rule.noncurrent_version_expiration = Some(NoncurrentExpirationDoc {
                noncurrent_days: Some(days(&self.noncurrent_days, "Versions non actuelles")?),
                newer_noncurrent_versions: newer,
            });
        } else {
            rule.noncurrent_version_expiration = None;
        }

        rule.abort_incomplete_multipart_upload = if self.abort {
            Some(AbortDoc {
                days_after_initiation: days(&self.abort_days, "Envois incomplets")?,
            })
        } else {
            None
        };
        rule.validate().map_err(|error| format!("{error:#}"))?;
        Ok(rule)
    }
}

/// The whole configuration from the forms, or the first problem found.
pub fn doc_from_forms(forms: &[RuleForm]) -> Result<LifecycleDoc, String> {
    let rules = forms
        .iter()
        .enumerate()
        .map(|(index, form)| {
            form.to_doc().map_err(|error| {
                let label = form.base.label(index);
                let label = if form.id.trim().is_empty() { format!("Règle {}", index + 1) } else { label };
                format!("{label} : {error}")
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let doc = LifecycleDoc { rules };
    doc.validate().map_err(|error| format!("{error:#}"))?;
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple_form() -> RuleForm {
        RuleForm {
            id: "logs".to_owned(),
            prefix: "logs/".to_owned(),
            expire: true,
            expire_days: "30".to_owned(),
            noncurrent: true,
            noncurrent_days: "7".to_owned(),
            abort: true,
            abort_days: "2".to_owned(),
            ..RuleForm::default()
        }
    }

    #[test]
    fn a_simple_rule_is_built_from_the_form() {
        let rule = simple_form().to_doc().unwrap();
        assert_eq!(rule.id.as_deref(), Some("logs"));
        assert_eq!(rule.filter.as_ref().and_then(|f| f.prefix.as_deref()), Some("logs/"));
        assert_eq!(rule.expiration.as_ref().and_then(|e| e.days), Some(30));
        assert_eq!(rule.noncurrent_version_expiration.as_ref().and_then(|n| n.noncurrent_days), Some(7));
        assert_eq!(rule.abort_incomplete_multipart_upload.as_ref().map(|a| a.days_after_initiation), Some(2));
        assert_eq!(
            rule.summary(),
            "expire après 30 jours · anciennes versions supprimées après 7 jours · envois incomplets interrompus après 2 jours"
        );

        let sdk = LifecycleDoc { rules: vec![rule.clone()] }.to_sdk().unwrap().unwrap();
        let sent = &sdk.rules()[0];
        assert_eq!(sent.id(), Some("logs"));
        assert_eq!(sent.filter().and_then(|f| f.prefix()), Some("logs/"));
        assert_eq!(sent.expiration().and_then(|e| e.days()), Some(30));
        assert_eq!(*sent.status(), ExpirationStatus::Enabled);
        // And back, as a server would return it.
        assert_eq!(LifecycleDoc::from_sdk(sdk.rules()).rules, [rule]);
    }

    #[test]
    fn the_form_reports_what_is_wrong() {
        let mut form = simple_form();
        form.expire_days = "0".to_owned();
        assert!(form.to_doc().unwrap_err().contains("Expiration"));
        form.expire_days = "abc".to_owned();
        assert!(form.to_doc().is_err());

        let nothing = RuleForm {
            expire: false,
            noncurrent: false,
            abort: false,
            ..simple_form()
        };
        assert!(nothing.to_doc().unwrap_err().contains("au moins une action"));

        let twice = doc_from_forms(&[simple_form(), simple_form()]).unwrap_err();
        assert!(twice.contains("Deux règles"), "{twice}");
    }

    #[test]
    fn no_prefix_means_the_whole_bucket() {
        let form = RuleForm {
            prefix: String::new(),
            ..simple_form()
        };
        let rule = form.to_doc().unwrap();
        assert_eq!(rule.filter, None);
        let sdk = LifecycleDoc { rules: vec![rule] }.to_sdk().unwrap().unwrap();
        // An empty filter, which S3 requires, not a missing one.
        assert_eq!(sdk.rules()[0].filter().and_then(|f| f.prefix()), Some(""));
        assert_eq!(LifecycleDoc::from_sdk(sdk.rules()).rules[0].filter, None);
    }

    #[test]
    fn what_the_form_does_not_show_is_kept() {
        let json = r#"{
            "Rules": [{
                "ID": "archive",
                "Filter": {"And": {"Prefix": "data/", "Tags": [{"Key": "tier", "Value": "cold"}]}},
                "Status": "Enabled",
                "Transitions": [{"Days": 30, "StorageClass": "GLACIER"}],
                "NoncurrentVersionExpiration": {"NoncurrentDays": 90, "NewerNoncurrentVersions": 3}
            }]
        }"#;
        let doc = LifecycleDoc::from_json(json).unwrap();
        let mut form = RuleForm::from_doc(&doc.rules[0]);
        assert!(!form.prefix_editable());
        assert_eq!(form.prefix, "");
        assert!(form.noncurrent && form.noncurrent_days == "90");
        assert_eq!(
            doc.rules[0].advanced(),
            ["filtre avancé", "transitions", "versions récentes conservées"]
        );

        form.noncurrent_days = "120".to_owned();
        form.enabled = false;
        let edited = form.to_doc().unwrap();
        assert_eq!(edited.filter, doc.rules[0].filter);
        assert_eq!(edited.transitions, doc.rules[0].transitions);
        assert_eq!(edited.status, Status::Disabled);
        assert_eq!(
            edited.noncurrent_version_expiration,
            Some(NoncurrentExpirationDoc {
                noncurrent_days: Some(120),
                newer_noncurrent_versions: Some(3)
            })
        );

        // The whole document goes through the SDK types and back unchanged.
        let edited_doc = LifecycleDoc { rules: vec![edited] };
        let sdk = edited_doc.to_sdk().unwrap().unwrap();
        assert_eq!(LifecycleDoc::from_sdk(sdk.rules()), edited_doc);
    }

    #[test]
    fn json_round_trips_in_the_aws_cli_shape() {
        let doc = LifecycleDoc {
            rules: vec![simple_form().to_doc().unwrap()],
        };
        let json = doc.to_json();
        assert!(json.contains("\"ID\": \"logs\""), "{json}");
        assert!(json.contains("\"DaysAfterInitiation\": 2"), "{json}");
        assert!(!json.contains("Transitions"), "empty lists are left out: {json}");
        assert_eq!(LifecycleDoc::from_json(&json).unwrap(), doc);
        assert_eq!(LifecycleDoc::from_json("  ").unwrap(), LifecycleDoc::default());
    }

    #[test]
    fn bad_json_says_where() {
        let error = LifecycleDoc::from_json("{\"Rules\": [{\"Status\": \"Enabled\", \"Expirations\": {}}]}")
            .unwrap_err()
            .to_string();
        assert!(error.contains("ligne 1") && error.contains("Expirations"), "{error}");
        let error = LifecycleDoc::from_json("{\"Rules\": [").unwrap_err().to_string();
        assert!(error.starts_with("JSON invalide"), "{error}");

        let dated = LifecycleDoc::from_json(
            r#"{"Rules": [{"Status": "Enabled", "Expiration": {"Date": "demain"}}]}"#,
        )
        .unwrap();
        let error = format!("{:#}", dated.validate().unwrap_err());
        assert!(error.contains("demain"), "{error}");
        let both = LifecycleDoc::from_json(
            r#"{"Rules": [{"Status": "Enabled", "Expiration": {"Days": 3, "ExpiredObjectDeleteMarker": true}}]}"#,
        )
        .unwrap();
        assert!(format!("{:#}", both.validate().unwrap_err()).contains("un seul"));
    }

    #[test]
    fn dates_go_through() {
        let doc = LifecycleDoc::from_json(
            r#"{"Rules": [{"ID": "end", "Status": "Enabled", "Expiration": {"Date": "2027-01-01T00:00:00Z"}}]}"#,
        )
        .unwrap();
        let sdk = doc.to_sdk().unwrap().unwrap();
        assert_eq!(LifecycleDoc::from_sdk(sdk.rules()).rules[0].expiration, doc.rules[0].expiration);
        assert_eq!(doc.rules[0].advanced(), ["expiration à date fixe"]);
    }
}
