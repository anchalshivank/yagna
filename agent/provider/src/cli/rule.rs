use crate::rules::outbound::{CertRule, Mode, OutboundConfig, OutboundRule};
use crate::rules::restrict::{RestrictRule, RuleAccessor};
use crate::{rules::RulesManager, startup_config::ProviderConfig};

use anyhow::{bail, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use structopt::StructOpt;
use strum::VariantNames;

use ya_client_model::NodeId;
use ya_manifest_utils::keystore::{AddParams, AddResponse, Keystore};
use ya_manifest_utils::short_cert_ids::shorten_cert_ids;
use ya_utils_cli::{CommandOutput, ResponseTable};

#[derive(StructOpt, Clone, Debug)]
pub enum RuleCommand {
    /// Set Modes for specific Rules
    Set(SetRule),
    /// Add new rule.
    Add(AddRule),
    /// Remove existing rule.
    Remove(RemoveRule),
    /// Enable all rules in category.
    Enable(RuleCategory),
    /// Disable all rules in category.
    Disable(RuleCategory),
    /// List active Rules and their information
    List,
}

/// Left for compatibility only. Should be replaced by AddRule and RemoveRule.
#[derive(StructOpt, Clone, Debug)]
pub enum SetRule {
    Outbound(SetOutboundRule),
}

#[derive(StructOpt, Clone, Debug)]
pub enum RuleCategory {
    Outbound,
    Blacklist,
    AllowOnly,
}

#[derive(StructOpt, Clone, Debug)]
pub enum AddRule {
    Outbound(SetOutboundRule),
    Blacklist(RestrictRuleDesc),
    AllowOnly(RestrictRuleDesc),
}

#[derive(StructOpt, Clone, Debug)]
pub enum RemoveRule {
    Outbound(SetOutboundRule),
    Blacklist(RestrictRuleDesc),
    AllowOnly(RestrictRuleDesc),
}

#[derive(StructOpt, Clone, Debug)]
pub enum SetOutboundRule {
    Disable,
    Enable,
    Everyone {
        #[structopt(short, long, possible_values = Mode::VARIANTS)]
        mode: Mode,
    },
    AuditedPayload(AuditedPayloadRuleWithCert),
    Partner(PartnerRuleWithCert),
}

#[derive(StructOpt, Clone, Debug)]
pub enum RestrictRuleDesc {
    ByNodeId {
        #[structopt(short, long)]
        address: NodeId,
    },
    Certified(RestrictRuleWithCert),
}

#[derive(StructOpt, Clone, Debug)]
pub struct CertId {
    /// Certificate id
    cert_id: String,
    #[structopt(short, long, possible_values = Mode::VARIANTS)]
    mode: Mode,
}

#[derive(StructOpt, Clone, Debug)]
pub enum AuditedPayloadRuleWithCert {
    /// Set rule for X509 certificate with given id.
    CertId(CertId),
    /// Import and set rule for X509 certificate or X509 certificates chain (rule will be assigned to last certificate in a chain).
    ImportCert {
        /// Path to X509 certificate or X509 certificates chain.
        imported_cert: PathBuf,
        #[structopt(short, long, possible_values = Mode::VARIANTS)]
        mode: Mode,
    },
}

#[derive(StructOpt, Clone, Debug)]
pub enum PartnerRuleWithCert {
    /// Set rule for Golem certificate with given id.
    CertId(CertId),
    /// Import and set rule for Golem certificate or Golem certificates chain.
    ImportCert {
        /// Path to Golem certificate.
        imported_cert: PathBuf,
        #[structopt(short, long, possible_values = Mode::VARIANTS)]
        mode: Mode,
    },
}

#[derive(StructOpt, Clone, Debug)]
pub enum RestrictRuleWithCert {
    /// Set rule for Golem certificate with given id.
    CertId {
        /// Certificate id
        cert_id: String,
    },
    /// Import and set rule for Golem certificate or Golem certificates chain.
    ImportCert {
        /// Path to Golem certificate.
        imported_cert: PathBuf,
    },
}

impl RuleCommand {
    pub fn run(self, config: ProviderConfig) -> Result<()> {
        let rules = RulesManager::load_or_create(
            &config.rules_file,
            &config.domain_whitelist_file,
            &config.cert_dir_path()?,
        )?;

        match self {
            RuleCommand::Set(set_rule) => set(set_rule, config),
            RuleCommand::List => list(config),
            RuleCommand::Add(add_rule) => add(add_rule, rules),
            RuleCommand::Remove(remove_rule) => remove(remove_rule, rules),
            RuleCommand::Enable(category) => enable(category, rules),
            RuleCommand::Disable(category) => disable(category, rules),
        }
    }
}

fn add(rule: AddRule, mut rules: RulesManager) -> Result<()> {
    match rule {
        AddRule::Outbound(_rule) => {
            bail!("Outbound rules are not supported yet by this command. Use `rule set` instead.")
        }
        AddRule::Blacklist(RestrictRuleDesc::ByNodeId { address }) => {
            rules.blacklist().add_identity_rule(address)
        }
        AddRule::Blacklist(RestrictRuleDesc::Certified(rule)) => match rule {
            RestrictRuleWithCert::CertId { cert_id } => {
                rules.blacklist().add_certified_rule(&cert_id)
            }
            RestrictRuleWithCert::ImportCert { imported_cert } => {
                let certs = rules.import_certs(&imported_cert)?;
                for cert in certs {
                    rules.blacklist().add_certified_rule(&cert)?;
                }
                Ok(())
            }
        },
        AddRule::AllowOnly(RestrictRuleDesc::ByNodeId { address }) => {
            rules.allow_only().add_identity_rule(address)
        }
        AddRule::AllowOnly(RestrictRuleDesc::Certified(rule)) => match rule {
            RestrictRuleWithCert::CertId { cert_id } => {
                rules.allow_only().add_certified_rule(&cert_id)
            }
            RestrictRuleWithCert::ImportCert { imported_cert } => {
                let certs = rules.import_certs(&imported_cert)?;
                for cert in certs {
                    rules.allow_only().add_certified_rule(&cert)?;
                }
                Ok(())
            }
        },
    }
}

fn remove(rule: RemoveRule, rules: RulesManager) -> Result<()> {
    match rule {
        RemoveRule::Outbound(_rule) => {
            bail!("Outbound rules are not supported yet by this command. Use `rule set` instead.")
        }
        RemoveRule::Blacklist(RestrictRuleDesc::ByNodeId { address }) => {
            rules.blacklist().remove_identity_rule(address)
        }
        RemoveRule::Blacklist(RestrictRuleDesc::Certified(rule)) => match rule {
            RestrictRuleWithCert::CertId { cert_id } => {
                rules.blacklist().remove_certified_rule(&cert_id)
            }
            RestrictRuleWithCert::ImportCert { .. } => bail!("Use cert id to remove rule"),
        },
        RemoveRule::AllowOnly(RestrictRuleDesc::ByNodeId { address }) => {
            rules.allow_only().remove_identity_rule(address)
        }
        RemoveRule::AllowOnly(RestrictRuleDesc::Certified(rule)) => match rule {
            RestrictRuleWithCert::CertId { cert_id } => {
                rules.allow_only().remove_certified_rule(&cert_id)
            }
            RestrictRuleWithCert::ImportCert { .. } => bail!("Use cert id to remove rule"),
        },
    }
}

fn enable(category: RuleCategory, rules: RulesManager) -> Result<()> {
    match category {
        RuleCategory::Outbound => rules.set_enabled(true),
        RuleCategory::Blacklist => rules.blacklist().enable(),
        RuleCategory::AllowOnly => rules.allow_only().enable(),
    }
}

fn disable(category: RuleCategory, rules: RulesManager) -> Result<()> {
    match category {
        RuleCategory::Outbound => rules.set_enabled(false),
        RuleCategory::Blacklist => rules.blacklist().disable(),
        RuleCategory::AllowOnly => rules.allow_only().disable(),
    }
}

fn set(set_rule: SetRule, config: ProviderConfig) -> Result<()> {
    let mut rules = RulesManager::load_or_create(
        &config.rules_file,
        &config.domain_whitelist_file,
        &config.cert_dir_path()?,
    )?;

    match set_rule {
        SetRule::Outbound(outbound) => match outbound {
            SetOutboundRule::Disable => rules.set_enabled(false),
            SetOutboundRule::Enable => rules.set_enabled(true),
            SetOutboundRule::Everyone { mode } => rules.set_everyone_mode(mode),
            SetOutboundRule::AuditedPayload(AuditedPayloadRuleWithCert::CertId(CertId {
                cert_id,
                mode,
            })) => rules.set_audited_payload_mode(cert_id, mode),
            SetOutboundRule::AuditedPayload(AuditedPayloadRuleWithCert::ImportCert {
                imported_cert: import_cert,
                mode,
            }) => {
                // TODO change it to `rules.keystore.add` when AuditedPayload will support Golem certs.
                let AddResponse {
                    invalid,
                    leaf_cert_ids,
                    duplicated,
                    ..
                } = rules.keystore.add_x509_cert(&AddParams {
                    certs: vec![import_cert],
                })?;

                for cert_path in invalid {
                    log::error!("Failed to import X509 certificates from: {cert_path:?}.");
                }

                rules.keystore.reload()?;

                if leaf_cert_ids.is_empty() && !duplicated.is_empty() {
                    log::warn!("Certificate is already in keystore- please use `cert-id` instead of `import-cert`");
                }

                for cert_id in leaf_cert_ids {
                    rules.set_audited_payload_mode(cert_id, mode.clone())?;
                }

                Ok(())
            }
            SetOutboundRule::Partner(PartnerRuleWithCert::CertId(CertId { cert_id, mode })) => {
                rules.set_partner_mode(cert_id, mode)
            }
            SetOutboundRule::Partner(PartnerRuleWithCert::ImportCert {
                imported_cert: import_cert,
                mode,
            }) => {
                let leaf_cert_ids = rules.import_certs(&import_cert)?;
                for cert_id in leaf_cert_ids {
                    rules.set_partner_mode(cert_id, mode.clone())?;
                }

                Ok(())
            }
        },
    }
}

fn list(config: ProviderConfig) -> Result<()> {
    let rules = RulesManager::load_or_create(
        &config.rules_file,
        &config.domain_whitelist_file,
        &config.cert_dir_path()?,
    )?;

    if config.json {
        rules.rulestore.print()?
    } else {
        let outbound_table = RulesTable::from(rules.clone());
        let blacklist_table = RulesTable::from(rules.clone().blacklist());
        let allowonly_table = RulesTable::from(rules.allow_only());

        outbound_table.print()?;
        blacklist_table.print()?;
        allowonly_table.print()?;
    };

    Ok(())
}

struct RulesTable {
    header: Option<String>,
    table: ResponseTable,
}

impl RulesTable {
    fn new() -> Self {
        let columns = vec![
            "rule".to_string(),
            "mode".to_string(),
            "certificate".to_string(),
            "description".to_string(),
        ];
        let values = vec![];
        let table = ResponseTable { columns, values };

        Self {
            header: None,
            table,
        }
    }

    fn with_header(&mut self, outbound_status: bool) {
        let status = if outbound_status {
            "enabled"
        } else {
            "disabled"
        };
        let header = format!("\nOutbound status: {status}");

        self.header = Some(header)
    }

    fn add_everyone(&mut self, outbound_everyone: &Mode) {
        let row = serde_json::json! {[ "Everyone", outbound_everyone, "", "" ]};
        self.table.values.push(row);
    }

    fn add_audited_payload(&mut self, audited_payload: &HashMap<String, CertRule>) {
        let rules: Vec<_> = audited_payload.iter().collect();
        let long_ids: Vec<String> = rules.iter().map(|e| e.0.clone()).collect();
        let short_ids = shorten_cert_ids(&long_ids);

        for ((_long_id, rule), short_id) in rules.into_iter().zip(short_ids) {
            let row = serde_json::json! {[ OutboundRule::AuditedPayload, rule.mode, short_id, rule.description ]};
            self.table.values.push(row);
        }
    }

    fn add_partner(&mut self, partner: &HashMap<String, CertRule>) {
        let rules: Vec<_> = partner.iter().collect();
        let long_ids: Vec<String> = rules.iter().map(|e| e.0.clone()).collect();
        let short_ids = shorten_cert_ids(&long_ids);

        for ((_long_id, rule), short_id) in rules.into_iter().zip(short_ids) {
            let row = serde_json::json! {[ OutboundRule::Partner, rule.mode, short_id, rule.description ]};
            self.table.values.push(row);
        }
    }

    pub fn print(self) -> Result<()> {
        let output = CommandOutput::Table {
            columns: self.table.columns,
            values: self.table.values,
            summary: vec![],
            header: self.header,
        };

        output.print(false)?;

        Ok(())
    }
}

impl From<RulesManager> for RulesTable {
    fn from(rules: RulesManager) -> Self {
        let mut table = RulesTable::new();
        let outbound = &rules.rulestore.config.read().unwrap().outbound;

        table.with_header(outbound.enabled);
        table.add_everyone(&outbound.everyone);
        table.add_audited_payload(&outbound.audited_payload);
        table.add_partner(&outbound.partner);

        table
    }
}

impl<G: TablePrint> From<G> for RulesTable {
    fn from(rules: G) -> Self {
        let mut table = RulesTable::new();

        table.header = Some(rules.header());
        table.table.columns = rules.columns();
        table.table.values = rules.rows();
        table
    }
}

pub trait TablePrint {
    fn header(&self) -> String;
    fn columns(&self) -> Vec<String>;
    fn rows(&self) -> Vec<serde_json::Value>;
}

impl TablePrint for OutboundConfig {
    fn header(&self) -> String {
        let status = if self.enabled { "enabled" } else { "disabled" };
        return format!("\nOutbound status: {status}");
    }

    fn columns(&self) -> Vec<String> {
        return vec![
            "rule".to_string(),
            "mode".to_string(),
            "certificate".to_string(),
            "description".to_string(),
        ];
    }

    fn rows(&self) -> Vec<Value> {
        add_everyone(&self.everyone)
            .into_iter()
            .chain(add_audited_payload(&self.audited_payload))
            .chain(add_partner(&self.partner))
            .collect()
    }
}

fn add_everyone(outbound_everyone: &Mode) -> Vec<Value> {
    vec![serde_json::json! {[ "Everyone", outbound_everyone, "", "" ]}]
}

fn add_audited_payload<'a>(
    audited_payload: &'a HashMap<String, CertRule>,
) -> impl Iterator<Item = Value> + 'a {
    let rules: Vec<_> = audited_payload.iter().collect();
    let long_ids: Vec<String> = rules.iter().map(|e| e.0.clone()).collect();
    let short_ids = shorten_cert_ids(&long_ids);

    rules.into_iter().zip(short_ids).map(|((_long_id, rule), short_id)| {
        serde_json::json! {[ OutboundRule::AuditedPayload, rule.mode, short_id, rule.description ]}
    })
}

fn add_partner<'a>(partner: &'a HashMap<String, CertRule>) -> impl Iterator<Item = Value> + 'a {
    let rules: Vec<_> = partner.iter().collect();
    let long_ids: Vec<String> = rules.iter().map(|e| e.0.clone()).collect();
    let short_ids = shorten_cert_ids(&long_ids);

    rules
        .into_iter()
        .zip(short_ids)
        .map(|((_long_id, rule), short_id)| {
            serde_json::json! {[ OutboundRule::Partner, rule.mode, short_id, rule.description ]}
        })
}

impl<G: RuleAccessor> TablePrint for RestrictRule<G> {
    fn header(&self) -> String {
        let status = if self.is_enabled() {
            "enabled"
        } else {
            "disabled"
        };
        return format!("\n{} status: {status}", G::rule_name());
    }

    fn columns(&self) -> Vec<String> {
        return vec![
            "rule".to_string(),
            "node".to_string(),
            "certificate".to_string(),
            "description".to_string(),
        ];
    }

    fn rows(&self) -> Vec<Value> {
        let nodes = self.list_identities();
        let long_ids = self.list_certs();
        let short_ids = shorten_cert_ids(&long_ids);

        long_ids
            .into_iter()
            .zip(short_ids)
            .map(|(_long_id, short_id)| {
                serde_json::json! {[ "Certified", "", short_id, "" ]}
            })
            .chain(nodes.into_iter().map(|node| {
                serde_json::json! {[ "ByNodeId", node, "", "" ]}
            }))
            .collect()
    }
}
