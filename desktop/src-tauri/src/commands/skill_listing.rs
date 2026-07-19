use std::collections::BTreeSet;

use csswitch_skill_install_core::{
    active_org, inspect_active_org_skills, read_agent_skill_names, InstalledSkillSource,
    SkillListWarning,
};
use serde::Serialize;
use tauri::State;

use crate::runtime::external_skill_route;
use crate::runtime::science::{
    probe_sandbox_runtime_cached, sandbox_data_dir, SandboxScienceState,
};
use crate::{lock, run_blocking, SharedAppState};

const LIST_SCHEMA_VERSION: u64 = 1;
const MAX_LIST_WARNINGS: usize = 2_000;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ScienceState {
    RunningHealthy,
    Stopped,
    Unverified,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ActiveOrgState {
    Ready,
    Missing,
    Invalid,
    Changed,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum AttachmentReadback {
    Verified,
    Unavailable,
    Failed,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum AttachmentState {
    Attached,
    Detached,
    Unknown,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
struct SkillListItem {
    skill_id: String,
    display_name: String,
    description: Option<String>,
    source_kind: InstalledSkillSource,
    bundle_name: Option<String>,
    attachment_state: AttachmentState,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct SkillListResponse {
    schema_version: u64,
    science_state: ScienceState,
    active_org_state: ActiveOrgState,
    attachment_readback: AttachmentReadback,
    agent_name: &'static str,
    items: Vec<SkillListItem>,
    warnings: Vec<SkillListWarning>,
}

#[tauri::command]
pub(crate) async fn list_installed_skills(
    state: State<'_, SharedAppState>,
) -> Result<SkillListResponse, String> {
    let state = state.inner().clone();
    run_blocking(move || build_skill_list(&state)).await
}

fn build_skill_list(state: &SharedAppState) -> Result<SkillListResponse, String> {
    let data_dir = sandbox_data_dir();
    let active_path = data_dir.join("active-org.json");
    let active_file_exists = std::fs::symlink_metadata(&active_path).is_ok();
    let mut response = SkillListResponse {
        schema_version: LIST_SCHEMA_VERSION,
        science_state: ScienceState::Unverified,
        active_org_state: if active_file_exists {
            ActiveOrgState::Invalid
        } else {
            ActiveOrgState::Missing
        },
        attachment_readback: AttachmentReadback::Unavailable,
        agent_name: "OPERON",
        items: Vec::new(),
        warnings: Vec::new(),
    };

    let snapshot = match inspect_active_org_skills(&data_dir) {
        Ok(snapshot) => {
            response.active_org_state = ActiveOrgState::Ready;
            Some(snapshot)
        }
        Err(error) => {
            if error.code == "ACTIVE_ORG_CHANGED" {
                response.active_org_state = ActiveOrgState::Changed;
            }
            response.warnings.push(SkillListWarning {
                code: error.code,
                skill_id: None,
                message: error.message,
            });
            None
        }
    };
    let mut discard_snapshot = false;

    let (sandbox_port, version_cache) = {
        let locked = lock(state);
        (locked.sandbox_port, locked.science_version_cache.clone())
    };
    if sandbox_port == 0 {
        response.science_state = ScienceState::Unverified;
    } else {
        match probe_sandbox_runtime_cached(sandbox_port, &version_cache) {
            Ok((SandboxScienceState::RunningHealthy, Some(runtime))) => {
                response.science_state = ScienceState::RunningHealthy;
                if let Some(snapshot) = snapshot.as_ref() {
                    match runtime.skill_install_host_context(sandbox_port) {
                        Ok(context) => {
                            match read_agent_skill_names(&context, &snapshot.active_org) {
                                Ok(attached) => {
                                    response.attachment_readback = AttachmentReadback::Verified;
                                    response.items = snapshot
                                        .items
                                        .iter()
                                        .cloned()
                                        .map(|item| list_item(item, Some(&attached)))
                                        .collect();
                                }
                                Err(error) => {
                                    response.attachment_readback = AttachmentReadback::Failed;
                                    if error.code == "ACTIVE_ORG_CHANGED" {
                                        response.active_org_state = ActiveOrgState::Changed;
                                        discard_snapshot = true;
                                    }
                                    response.warnings.push(SkillListWarning {
                                        code: error.code,
                                        skill_id: None,
                                        message: "无法可信回读 OPERON 绑定状态，列表已标为未知"
                                            .to_string(),
                                    });
                                }
                            }
                        }
                        Err(_) => {
                            response.science_state = ScienceState::Unverified;
                            response.attachment_readback = AttachmentReadback::Failed;
                            response.warnings.push(SkillListWarning {
                                code: "SCIENCE_RUNTIME_UNVERIFIED".to_string(),
                                skill_id: None,
                                message: "Science runtime 身份不完整，未回读绑定状态".to_string(),
                            });
                        }
                    }
                }
            }
            Ok((SandboxScienceState::Stopped, _)) => {
                response.science_state = ScienceState::Stopped;
            }
            Ok((SandboxScienceState::Unknown, _))
            | Ok((SandboxScienceState::RunningHealthy, None)) => {
                response.science_state = ScienceState::Unverified;
            }
            Err(_) => {
                response.science_state = ScienceState::Unverified;
            }
        }
    }

    if let Some(snapshot) = snapshot {
        if discard_snapshot || !active_org_matches(&data_dir, &snapshot.active_org) {
            mark_active_org_changed(&mut response);
        } else {
            if response.items.is_empty() && !snapshot.items.is_empty() {
                response.items = snapshot
                    .items
                    .iter()
                    .cloned()
                    .map(|item| list_item(item, None))
                    .collect();
            }
            response.warnings.extend(snapshot.warnings);
            verify_system_route_source(&data_dir, &snapshot.active_org, &mut response);
            if !active_org_matches(&data_dir, &snapshot.active_org) {
                mark_active_org_changed(&mut response);
            }
        }
    }
    cap_response_warnings(&mut response);
    Ok(response)
}

fn cap_response_warnings(response: &mut SkillListResponse) {
    if response.warnings.len() <= MAX_LIST_WARNINGS {
        return;
    }
    response.warnings.truncate(MAX_LIST_WARNINGS - 1);
    response.warnings.push(SkillListWarning {
        code: "SKILL_WARNINGS_TRUNCATED".to_string(),
        skill_id: None,
        message: "其余 Skill 列表警告已按安全上限截断".to_string(),
    });
}

fn active_org_matches(data_dir: &std::path::Path, expected_org: &str) -> bool {
    active_org(data_dir).is_ok_and(|current| current == expected_org)
}

fn mark_active_org_changed(response: &mut SkillListResponse) {
    response.active_org_state = ActiveOrgState::Changed;
    response.attachment_readback = AttachmentReadback::Failed;
    response.items.clear();
    response.warnings.clear();
    response.warnings.push(SkillListWarning {
        code: "ACTIVE_ORG_CHANGED".to_string(),
        skill_id: None,
        message: "Science active org 在列表读取期间发生变化".to_string(),
    });
}

fn list_item(
    item: csswitch_skill_install_core::InstalledSkillSummary,
    attached: Option<&BTreeSet<String>>,
) -> SkillListItem {
    let attachment_state = match attached {
        Some(names) if names.contains(&item.skill_id) => AttachmentState::Attached,
        Some(_) => AttachmentState::Detached,
        None => AttachmentState::Unknown,
    };
    SkillListItem {
        skill_id: item.skill_id,
        display_name: item.display_name,
        description: item.description,
        source_kind: item.source_kind,
        bundle_name: item.bundle_name,
        attachment_state,
    }
}

fn verify_system_route_source(
    data_dir: &std::path::Path,
    expected_org: &str,
    response: &mut SkillListResponse,
) {
    let Some(item) = response
        .items
        .iter_mut()
        .find(|item| item.skill_id == external_skill_route::SKILL_NAME)
    else {
        return;
    };
    if external_skill_route::inspect_route_skill_for_org(data_dir, expected_org).unwrap_or(false) {
        item.source_kind = InstalledSkillSource::CsswitchSystem;
        response.warnings.retain(|warning| {
            warning.skill_id.as_deref() != Some(external_skill_route::SKILL_NAME)
                || warning.code != "SKILL_SOURCE_UNVERIFIED"
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use csswitch_skill_install_core::InstalledSkillSummary;
    #[cfg(feature = "acceptance-build")]
    use std::sync::{Arc, Mutex};

    fn summary(name: &str) -> InstalledSkillSummary {
        InstalledSkillSummary {
            skill_id: name.to_string(),
            display_name: name.to_string(),
            description: None,
            source_kind: InstalledSkillSource::ScienceLocal,
            bundle_name: None,
        }
    }

    #[test]
    fn attachment_state_is_three_valued() {
        let attached = BTreeSet::from(["one".to_string()]);
        assert_eq!(
            list_item(summary("one"), Some(&attached)).attachment_state,
            AttachmentState::Attached
        );
        assert_eq!(
            list_item(summary("two"), Some(&attached)).attachment_state,
            AttachmentState::Detached
        );
        assert_eq!(
            list_item(summary("one"), None).attachment_state,
            AttachmentState::Unknown
        );
    }

    #[test]
    fn serialized_contract_does_not_expose_path_or_active_org() {
        let response = SkillListResponse {
            schema_version: 1,
            science_state: ScienceState::Stopped,
            active_org_state: ActiveOrgState::Ready,
            attachment_readback: AttachmentReadback::Unavailable,
            agent_name: "OPERON",
            items: vec![list_item(summary("demo"), None)],
            warnings: vec![],
        };
        let value = serde_json::to_value(response).unwrap();
        assert!(value.get("active_org").is_none());
        assert!(value["items"][0].get("path").is_none());
        assert_eq!(value["items"][0]["attachment_state"], "unknown");
    }

    #[test]
    fn warning_contract_is_capped_with_a_truncation_marker() {
        let mut response = SkillListResponse {
            schema_version: 1,
            science_state: ScienceState::Unverified,
            active_org_state: ActiveOrgState::Ready,
            attachment_readback: AttachmentReadback::Failed,
            agent_name: "OPERON",
            items: vec![],
            warnings: (0..=MAX_LIST_WARNINGS)
                .map(|index| SkillListWarning {
                    code: format!("WARNING_{index}"),
                    skill_id: None,
                    message: "bounded warning".to_string(),
                })
                .collect(),
        };
        cap_response_warnings(&mut response);
        assert_eq!(response.warnings.len(), MAX_LIST_WARNINGS);
        assert_eq!(
            response.warnings.last().unwrap().code,
            "SKILL_WARNINGS_TRUNCATED"
        );
    }

    #[test]
    fn changed_active_org_discards_snapshot_items() {
        let mut response = SkillListResponse {
            schema_version: 1,
            science_state: ScienceState::RunningHealthy,
            active_org_state: ActiveOrgState::Ready,
            attachment_readback: AttachmentReadback::Verified,
            agent_name: "OPERON",
            items: vec![list_item(summary("old-org-skill"), None)],
            warnings: vec![SkillListWarning {
                code: "OLD_ORG_WARNING".to_string(),
                skill_id: Some("old-org-skill".to_string()),
                message: "must be discarded".to_string(),
            }],
        };
        mark_active_org_changed(&mut response);
        assert_eq!(response.active_org_state, ActiveOrgState::Changed);
        assert_eq!(response.attachment_readback, AttachmentReadback::Failed);
        assert!(response.items.is_empty());
        assert_eq!(response.warnings.len(), 1);
        assert_eq!(response.warnings[0].code, "ACTIVE_ORG_CHANGED");
    }

    #[cfg(feature = "acceptance-build")]
    #[test]
    #[ignore = "explicit temp-HOME Acceptance command proof"]
    fn isolated_acceptance_command_lists_only_temp_home_skills() {
        let home = std::env::var("HOME").expect("explicit test HOME");
        let home_path = std::path::Path::new(&home);
        assert_eq!(home_path.parent(), Some(crate::test_temp_root().as_path()));
        assert!(home_path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("csswitch-skill-list-tauri-")));
        let data_dir = sandbox_data_dir();
        assert!(data_dir.starts_with(&home));
        std::fs::create_dir_all(data_dir.join("orgs/org-test/skills/demo")).unwrap();
        std::fs::write(
            data_dir.join("active-org.json"),
            r#"{"org_uuid":"org-test"}"#,
        )
        .unwrap();
        std::fs::write(
            data_dir.join("orgs/org-test/skills/demo/SKILL.md"),
            "---\nname: Isolated demo\ndescription: Temp HOME only\n---\n",
        )
        .unwrap();

        let state = Arc::new(Mutex::new(crate::AppState::default()));
        let response = build_skill_list(&state).unwrap();
        assert_eq!(response.active_org_state, ActiveOrgState::Ready);
        assert_eq!(response.science_state, ScienceState::Unverified);
        assert_eq!(
            response.attachment_readback,
            AttachmentReadback::Unavailable
        );
        assert_eq!(response.items.len(), 1);
        assert_eq!(response.items[0].skill_id, "demo");
        assert_eq!(response.items[0].display_name, "Isolated demo");
        assert_eq!(response.items[0].attachment_state, AttachmentState::Unknown);
    }
}
