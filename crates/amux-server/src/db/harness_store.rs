//! Store helpers for the production harness contracts (migration 0058).

use amux_core::harness::{
    GoalContract, GuideRule, GuideRuleStatus, HandoffPacket, PlanProjection, PlanningNode,
    PlanningNodeStatus, SensorProfile, TaskCheckpoint,
};
use amux_core::limits::ExecutionLimits;
use amux_core::policy::AgentRole;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use sha2::{Digest, Sha256};

fn corrupt(idx: usize, e: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(e))
}

fn invalid(message: &str) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        message.to_string(),
    )))
}

fn json<T: serde::Serialize>(value: &T) -> rusqlite::Result<String> {
    serde_json::to_string(value).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
}

fn from_json<T: serde::de::DeserializeOwned>(idx: usize, value: &str) -> rusqlite::Result<T> {
    serde_json::from_str(value).map_err(|e| corrupt(idx, e))
}

pub fn get_checkpoint(
    conn: &Connection,
    task_id: &str,
) -> rusqlite::Result<Option<TaskCheckpoint>> {
    conn.query_row(
        "SELECT task_id, version, completed_steps, next_action, artifacts, unresolved,
                input_hash, context_hash, last_result, updated_by, updated_at
           FROM _amux_task_checkpoints WHERE task_id=?1",
        [task_id],
        |r| {
            let at: String = r.get(10)?;
            Ok(TaskCheckpoint {
                task_id: r.get(0)?,
                version: r.get::<_, i64>(1)? as u32,
                completed_steps: from_json(2, &r.get::<_, String>(2)?)?,
                next_action: r.get(3)?,
                artifacts: from_json(4, &r.get::<_, String>(4)?)?,
                unresolved: from_json(5, &r.get::<_, String>(5)?)?,
                input_hash: r.get(6)?,
                context_hash: r.get(7)?,
                last_result: r.get(8)?,
                updated_by: r.get(9)?,
                updated_at: at.parse::<DateTime<Utc>>().map_err(|e| corrupt(10, e))?,
            })
        },
    )
    .optional()
}

/// Upsert a checkpoint and assign the next version in the transaction.
pub fn put_checkpoint(
    conn: &Connection,
    checkpoint: &TaskCheckpoint,
) -> rusqlite::Result<TaskCheckpoint> {
    checkpoint
        .validate()
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;
    let version: u32 = conn
        .query_row(
            "SELECT version + 1 FROM _amux_task_checkpoints WHERE task_id=?1",
            [&checkpoint.task_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(1);
    let mut stored = checkpoint.clone();
    stored.version = version;
    conn.execute(
        "INSERT INTO _amux_task_checkpoints
             (task_id,version,completed_steps,next_action,artifacts,unresolved,input_hash,
              context_hash,last_result,updated_by,updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
         ON CONFLICT(task_id) DO UPDATE SET version=?2,completed_steps=?3,next_action=?4,
             artifacts=?5,unresolved=?6,input_hash=?7,context_hash=?8,last_result=?9,
             updated_by=?10,updated_at=?11",
        params![
            stored.task_id,
            stored.version,
            json(&stored.completed_steps)?,
            stored.next_action,
            json(&stored.artifacts)?,
            json(&stored.unresolved)?,
            stored.input_hash,
            stored.context_hash,
            stored.last_result,
            stored.updated_by,
            stored.updated_at.to_rfc3339(),
        ],
    )?;
    Ok(stored)
}

fn handoff_from_row(r: &Row<'_>) -> rusqlite::Result<HandoffPacket> {
    let created: String = r.get(20)?;
    Ok(HandoffPacket {
        id: r.get(0)?,
        task_id: r.get(1)?,
        assignment_key: r.get(2)?,
        objective: r.get(3)?,
        criteria_version: r.get::<_, i64>(4)? as u32,
        checkpoint_version: r.get::<_, Option<i64>>(5)?.map(|v| v as u32),
        checkpoint_hash: r.get(6)?,
        artifacts: from_json(7, &r.get::<_, String>(7)?)?,
        evidence: from_json(8, &r.get::<_, String>(8)?)?,
        assumptions: from_json(9, &r.get::<_, String>(9)?)?,
        unresolved: from_json(10, &r.get::<_, String>(10)?)?,
        concerns: from_json(11, &r.get::<_, String>(11)?)?,
        deviations: from_json(12, &r.get::<_, String>(12)?)?,
        findings: from_json(13, &r.get::<_, String>(13)?)?,
        requires_replan: r.get::<_, i64>(14)? != 0,
        planning_scope_id: r.get(15)?,
        next_action: r.get(16)?,
        deadline: r.get(17)?,
        sender: r.get(18)?,
        receiver: r.get(19)?,
        created_at: created
            .parse::<DateTime<Utc>>()
            .map_err(|e| corrupt(20, e))?,
    })
}

const HANDOFF_COLS: &str = "id,task_id,assignment_key,objective,criteria_version,
    checkpoint_version,checkpoint_hash,artifacts,evidence,assumptions,unresolved,concerns,
    deviations,findings,requires_replan,planning_scope_id,next_action,deadline,sender,receiver,
    created_at";

pub fn insert_handoff(conn: &Connection, packet: &HandoffPacket) -> rusqlite::Result<bool> {
    packet
        .validate()
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;
    let n = conn.execute(
        &format!(
            "INSERT OR IGNORE INTO _amux_handoffs ({HANDOFF_COLS})
                  VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,
                          ?17,?18,?19,?20,?21)"
        ),
        params![
            packet.id,
            packet.task_id,
            packet.assignment_key,
            packet.objective,
            packet.criteria_version,
            packet.checkpoint_version,
            packet.checkpoint_hash,
            json(&packet.artifacts)?,
            json(&packet.evidence)?,
            json(&packet.assumptions)?,
            json(&packet.unresolved)?,
            json(&packet.concerns)?,
            json(&packet.deviations)?,
            json(&packet.findings)?,
            packet.requires_replan,
            packet.planning_scope_id,
            packet.next_action,
            packet.deadline,
            packet.sender,
            packet.receiver,
            packet.created_at.to_rfc3339(),
        ],
    )?;
    if n > 0 {
        if let Some(node_id) = packet.planning_scope_id.as_deref() {
            let node = get_planning_node(conn, node_id)?
                .ok_or_else(|| invalid("unknown planning node"))?;
            if node.owner != packet.sender {
                return Err(invalid("handoff sender does not own the planning node"));
            }
            if packet.requires_replan {
                let parent_id = node.parent_id.as_deref().ok_or_else(|| {
                    invalid("a root planning node cannot request parent replanning")
                })?;
                let parent = get_planning_node(conn, parent_id)?
                    .ok_or_else(|| invalid("planning node parent is missing"))?;
                if parent.owner != packet.receiver {
                    return Err(invalid(
                        "handoff receiver must be the parent planning-node owner",
                    ));
                }
                conn.execute(
                    "UPDATE _amux_planning_nodes
                     SET status='needs_replan',wake_count=wake_count+1,updated_at=?2 WHERE id=?1",
                    params![parent_id, packet.created_at.to_rfc3339()],
                )?;
            }
            conn.execute(
                "UPDATE _amux_planning_nodes SET status='complete',updated_at=?2 WHERE id=?1",
                params![node_id, packet.created_at.to_rfc3339()],
            )?;
        }
    }
    Ok(n > 0)
}

pub fn latest_handoff(conn: &Connection, task_id: &str) -> rusqlite::Result<Option<HandoffPacket>> {
    conn.query_row(
        &format!("SELECT {HANDOFF_COLS} FROM _amux_handoffs WHERE task_id=?1 ORDER BY created_at DESC LIMIT 1"),
        [task_id],
        handoff_from_row,
    )
    .optional()
}

fn goal_from_row(r: &Row<'_>) -> rusqlite::Result<GoalContract> {
    let updated_at: String = r.get(12)?;
    Ok(GoalContract {
        id: r.get(0)?,
        objective: r.get(1)?,
        non_goals: from_json(2, &r.get::<_, String>(2)?)?,
        success_metrics: from_json(3, &r.get::<_, String>(3)?)?,
        performance_requirements: from_json(4, &r.get::<_, String>(4)?)?,
        resource_constraints: from_json(5, &r.get::<_, String>(5)?)?,
        dependency_policy: r.get(6)?,
        release_policy: r.get(7)?,
        expected_scope_min: r.get::<_, i64>(8)? as u32,
        expected_scope_max: r.get::<_, i64>(9)? as u32,
        version: r.get::<_, i64>(10)? as u32,
        updated_by: r.get(11)?,
        updated_at: updated_at
            .parse::<DateTime<Utc>>()
            .map_err(|e| corrupt(12, e))?,
    })
}

const GOAL_COLS: &str = "id,objective,non_goals,success_metrics,performance_requirements,
    resource_constraints,dependency_policy,release_policy,expected_scope_min,expected_scope_max,
    version,updated_by,updated_at";

pub fn get_goal_contract(conn: &Connection, id: &str) -> rusqlite::Result<Option<GoalContract>> {
    conn.query_row(
        &format!("SELECT {GOAL_COLS} FROM _amux_goal_contracts WHERE id=?1"),
        [id],
        goal_from_row,
    )
    .optional()
}

/// Replace the current goal contract and append the exact resulting version
/// to immutable history in the same transaction.
pub fn put_goal_contract(
    conn: &Connection,
    contract: &GoalContract,
) -> rusqlite::Result<GoalContract> {
    contract.validate().map_err(invalid)?;
    let next: u32 = conn
        .query_row(
            "SELECT version + 1 FROM _amux_goal_contracts WHERE id=?1",
            [&contract.id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(1);
    let mut stored = contract.clone();
    stored.version = next;
    conn.execute(
        "INSERT INTO _amux_goal_contracts
         (id,objective,non_goals,success_metrics,performance_requirements,resource_constraints,
          dependency_policy,release_policy,expected_scope_min,expected_scope_max,version,
          updated_by,updated_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
         ON CONFLICT(id) DO UPDATE SET objective=?2,non_goals=?3,success_metrics=?4,
          performance_requirements=?5,resource_constraints=?6,dependency_policy=?7,
          release_policy=?8,expected_scope_min=?9,expected_scope_max=?10,version=?11,
          updated_by=?12,updated_at=?13",
        params![
            stored.id,
            stored.objective,
            json(&stored.non_goals)?,
            json(&stored.success_metrics)?,
            json(&stored.performance_requirements)?,
            json(&stored.resource_constraints)?,
            stored.dependency_policy,
            stored.release_policy,
            stored.expected_scope_min,
            stored.expected_scope_max,
            stored.version,
            stored.updated_by,
            stored.updated_at.to_rfc3339(),
        ],
    )?;
    conn.execute(
        "INSERT INTO _amux_goal_contract_revisions(goal_id,version,body,created_at)
         VALUES(?1,?2,?3,?4)",
        params![
            stored.id,
            stored.version,
            json(&stored)?,
            stored.updated_at.to_rfc3339()
        ],
    )?;
    Ok(stored)
}

pub fn goal_contract_history(conn: &Connection, id: &str) -> rusqlite::Result<Vec<GoalContract>> {
    let mut stmt = conn.prepare(
        "SELECT body FROM _amux_goal_contract_revisions WHERE goal_id=?1 ORDER BY version",
    )?;
    let rows = stmt
        .query_map([id], |r| from_json(0, &r.get::<_, String>(0)?))?
        .collect();
    rows
}

fn planning_node_from_row(r: &Row<'_>) -> rusqlite::Result<PlanningNode> {
    let role: String = r.get(4)?;
    let status: String = r.get(8)?;
    let updated_at: String = r.get(11)?;
    Ok(PlanningNode {
        id: r.get(0)?,
        goal_id: r.get(1)?,
        parent_id: r.get(2)?,
        task_id: r.get(3)?,
        role: from_json(4, &format!("\"{role}\""))?,
        owner: r.get(5)?,
        title: r.get(6)?,
        objective: r.get(7)?,
        status: from_json(8, &format!("\"{status}\""))?,
        current_plan_version: r.get::<_, i64>(9)? as u32,
        wake_count: r.get::<_, i64>(10)? as u32,
        updated_at: updated_at
            .parse::<DateTime<Utc>>()
            .map_err(|e| corrupt(11, e))?,
    })
}

const PLANNING_NODE_COLS: &str = "id,goal_id,parent_id,task_id,role,owner,title,objective,status,
    current_plan_version,wake_count,updated_at";

pub fn get_planning_node(conn: &Connection, id: &str) -> rusqlite::Result<Option<PlanningNode>> {
    conn.query_row(
        &format!("SELECT {PLANNING_NODE_COLS} FROM _amux_planning_nodes WHERE id=?1"),
        [id],
        planning_node_from_row,
    )
    .optional()
}

pub fn list_planning_nodes(
    conn: &Connection,
    goal_id: &str,
) -> rusqlite::Result<Vec<PlanningNode>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {PLANNING_NODE_COLS} FROM _amux_planning_nodes
         WHERE goal_id=?1 ORDER BY parent_id,id"
    ))?;
    let rows = stmt.query_map([goal_id], planning_node_from_row)?.collect();
    rows
}

pub fn insert_planning_node(conn: &Connection, node: &PlanningNode) -> rusqlite::Result<bool> {
    node.validate().map_err(invalid)?;
    let existing_role: Option<String> = conn
        .query_row(
            "SELECT role FROM _amux_planning_nodes
             WHERE owner=?1 AND status!='complete' ORDER BY id LIMIT 1",
            [&node.owner],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(existing_role) = existing_role {
        let existing: AgentRole = from_json(0, &format!("\"{existing_role}\""))?;
        if existing.is_planner() != node.role.is_planner() {
            return Err(invalid(
                "an active planning identity cannot also own execution or verification work",
            ));
        }
    }
    if let Some(parent_id) = node.parent_id.as_deref() {
        let parent = get_planning_node(conn, parent_id)?
            .ok_or_else(|| invalid("planning node parent does not exist"))?;
        if parent.goal_id != node.goal_id {
            return Err(invalid(
                "planning node and parent must belong to the same goal",
            ));
        }
    } else if get_goal_contract(conn, &node.goal_id)?.is_none() {
        return Err(invalid("root planning node goal does not exist"));
    }
    let n = conn.execute(
        &format!(
            "INSERT OR IGNORE INTO _amux_planning_nodes ({PLANNING_NODE_COLS})
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)"
        ),
        params![
            node.id,
            node.goal_id,
            node.parent_id,
            node.task_id,
            token(node.role),
            node.owner,
            node.title,
            node.objective,
            token(node.status),
            node.current_plan_version,
            node.wake_count,
            node.updated_at.to_rfc3339(),
        ],
    )?;
    Ok(n > 0)
}

pub fn insert_delegated_planning_node(
    conn: &Connection,
    node: &PlanningNode,
    delegated_by: &str,
) -> rusqlite::Result<bool> {
    let parent_id = node
        .parent_id
        .as_deref()
        .ok_or_else(|| invalid("delegated planning node requires a parent"))?;
    let parent = get_planning_node(conn, parent_id)?
        .ok_or_else(|| invalid("planning node parent does not exist"))?;
    if parent.owner != delegated_by
        || !parent.role.is_planner()
        || parent.status == PlanningNodeStatus::Complete
    {
        return Err(invalid(
            "only the owning parent planner may delegate this node",
        ));
    }
    insert_planning_node(conn, node)
}

pub fn ensure_goal_root_owner(
    conn: &Connection,
    goal_id: &str,
    actor: &str,
) -> rusqlite::Result<()> {
    let owner: Option<String> = conn
        .query_row(
            "SELECT owner FROM _amux_planning_nodes
             WHERE goal_id=?1 AND parent_id IS NULL AND role='root_planner' LIMIT 1",
            [goal_id],
            |r| r.get(0),
        )
        .optional()?;
    match owner {
        Some(owner) if owner == actor => Ok(()),
        Some(_) => Err(invalid("only the owning root planner may revise this goal")),
        None => Err(invalid("goal root planning node does not exist")),
    }
}

pub fn goal_root_owner(conn: &Connection, goal_id: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT owner FROM _amux_planning_nodes
         WHERE goal_id=?1 AND parent_id IS NULL AND role='root_planner' LIMIT 1",
        [goal_id],
        |r| r.get(0),
    )
    .optional()
}

pub fn planning_role_for_actor(
    conn: &Connection,
    actor: &str,
) -> rusqlite::Result<Option<AgentRole>> {
    // Planning vs execution is an identity-level separation, deliberately
    // consistent across goals. `insert_planning_node` rejects mixed active
    // role classes, so this lookup cannot accidentally prefer a planner role
    // from one goal over legitimate worker authority in another.
    let value: Option<String> = conn
        .query_row(
            "SELECT role FROM _amux_planning_nodes
             WHERE owner=?1 AND status!='complete'
             ORDER BY CASE role WHEN 'root_planner' THEN 0 WHEN 'subplanner' THEN 1 ELSE 2 END,id
             LIMIT 1",
            [actor],
            |r| r.get(0),
        )
        .optional()?;
    value
        .map(|value| from_json(0, &format!("\"{value}\"")))
        .transpose()
}

fn plan_from_row(r: &Row<'_>) -> rusqlite::Result<PlanProjection> {
    let created_at: String = r.get(5)?;
    Ok(PlanProjection {
        planning_node_id: r.get(0)?,
        version: r.get::<_, i64>(1)? as u32,
        body: r.get(2)?,
        reason: r.get(3)?,
        authored_by: r.get(4)?,
        created_at: created_at
            .parse::<DateTime<Utc>>()
            .map_err(|e| corrupt(5, e))?,
    })
}

const PLAN_COLS: &str = "planning_node_id,version,body,reason,authored_by,created_at";

pub fn put_plan(conn: &Connection, plan: &PlanProjection) -> rusqlite::Result<PlanProjection> {
    plan.validate().map_err(invalid)?;
    let node = get_planning_node(conn, &plan.planning_node_id)?
        .ok_or_else(|| invalid("planning node does not exist"))?;
    if node.owner != plan.authored_by || !node.role.is_planner() {
        return Err(invalid("only the owning planner may revise this plan"));
    }
    let next = node.current_plan_version + 1;
    let mut stored = plan.clone();
    stored.version = next;
    conn.execute(
        "INSERT INTO _amux_plan_revisions
         (planning_node_id,version,body,reason,authored_by,created_at)
         VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            stored.planning_node_id,
            stored.version,
            stored.body,
            stored.reason,
            stored.authored_by,
            stored.created_at.to_rfc3339(),
        ],
    )?;
    conn.execute(
        "INSERT INTO _amux_current_plans
         (planning_node_id,version,body,reason,authored_by,created_at)
         VALUES(?1,?2,?3,?4,?5,?6)
         ON CONFLICT(planning_node_id) DO UPDATE SET version=?2,body=?3,reason=?4,
          authored_by=?5,created_at=?6",
        params![
            stored.planning_node_id,
            stored.version,
            stored.body,
            stored.reason,
            stored.authored_by,
            stored.created_at.to_rfc3339(),
        ],
    )?;
    conn.execute(
        "UPDATE _amux_planning_nodes
         SET current_plan_version=?2,status='active',updated_at=?3 WHERE id=?1",
        params![
            stored.planning_node_id,
            stored.version,
            stored.created_at.to_rfc3339()
        ],
    )?;
    Ok(stored)
}

pub fn current_plan(
    conn: &Connection,
    planning_node_id: &str,
) -> rusqlite::Result<Option<PlanProjection>> {
    conn.query_row(
        &format!("SELECT {PLAN_COLS} FROM _amux_current_plans WHERE planning_node_id=?1"),
        [planning_node_id],
        plan_from_row,
    )
    .optional()
}

pub fn plan_history(
    conn: &Connection,
    planning_node_id: &str,
) -> rusqlite::Result<Vec<PlanProjection>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {PLAN_COLS} FROM _amux_plan_revisions
         WHERE planning_node_id=?1 ORDER BY version"
    ))?;
    let rows = stmt.query_map([planning_node_id], plan_from_row)?.collect();
    rows
}

pub fn put_budget(
    conn: &Connection,
    task_id: &str,
    limits: &ExecutionLimits,
) -> rusqlite::Result<u32> {
    let version: u32 = conn
        .query_row(
            "SELECT version + 1 FROM _amux_task_budgets WHERE task_id=?1",
            [task_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(1);
    conn.execute(
        "INSERT INTO _amux_task_budgets(task_id,limits,version,updated_at) VALUES(?1,?2,?3,?4)
         ON CONFLICT(task_id) DO UPDATE SET limits=?2,version=?3,updated_at=?4",
        params![task_id, json(limits)?, version, Utc::now().to_rfc3339()],
    )?;
    Ok(version)
}

pub fn get_budget(
    conn: &Connection,
    task_id: &str,
) -> rusqlite::Result<Option<(ExecutionLimits, u32)>> {
    conn.query_row(
        "SELECT limits,version FROM _amux_task_budgets WHERE task_id=?1",
        [task_id],
        |r| {
            let raw: String = r.get(0)?;
            Ok((from_json(0, &raw)?, r.get::<_, i64>(1)? as u32))
        },
    )
    .optional()
}

pub fn put_sensor_profile(conn: &Connection, profile: &SensorProfile) -> rusqlite::Result<u32> {
    let version: u32 = conn
        .query_row(
            "SELECT version + 1 FROM _amux_sensor_profiles WHERE task_type=?1",
            [&profile.task_type],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(1);
    conn.execute(
        "INSERT INTO _amux_sensor_profiles(task_type,criteria,version,updated_at) VALUES(?1,?2,?3,?4)
         ON CONFLICT(task_type) DO UPDATE SET criteria=?2,version=?3,updated_at=?4",
        params![profile.task_type, json(&profile.criteria)?, version, profile.updated_at.to_rfc3339()],
    )?;
    Ok(version)
}

pub fn get_sensor_profile(
    conn: &Connection,
    task_type: &str,
) -> rusqlite::Result<Option<SensorProfile>> {
    conn.query_row(
        "SELECT criteria,version,updated_at FROM _amux_sensor_profiles WHERE task_type=?1",
        [task_type],
        |r| {
            let at: String = r.get(2)?;
            Ok(SensorProfile {
                task_type: task_type.to_string(),
                criteria: from_json(0, &r.get::<_, String>(0)?)?,
                version: r.get::<_, i64>(1)? as u32,
                updated_at: at.parse::<DateTime<Utc>>().map_err(|e| corrupt(2, e))?,
            })
        },
    )
    .optional()
}

pub fn list_sensor_profiles(conn: &Connection) -> rusqlite::Result<Vec<SensorProfile>> {
    let mut stmt =
        conn.prepare("SELECT task_type FROM _amux_sensor_profiles ORDER BY task_type")?;
    let ids: Vec<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    ids.iter()
        .filter_map(|id| get_sensor_profile(conn, id).transpose())
        .collect()
}

fn token<T: serde::Serialize>(value: T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn guide_from_row(r: &Row<'_>) -> rusqlite::Result<GuideRule> {
    let added: String = r.get(11)?;
    let validated: Option<String> = r.get(12)?;
    let expires: Option<String> = r.get(13)?;
    Ok(GuideRule {
        id: r.get(0)?,
        scope: r.get(1)?,
        name: r.get(2)?,
        content: r.get(3)?,
        owner: r.get(4)?,
        source_failure: r.get(5)?,
        rationale: r.get(6)?,
        status: from_json(7, &format!("\"{}\"", r.get::<_, String>(7)?))?,
        enforcement_layer: from_json(8, &format!("\"{}\"", r.get::<_, String>(8)?))?,
        sensor_ref: r.get(9)?,
        version: r.get::<_, i64>(10)? as u32,
        added_at: added.parse::<DateTime<Utc>>().map_err(|e| corrupt(11, e))?,
        last_validated_at: validated
            .map(|v| v.parse::<DateTime<Utc>>())
            .transpose()
            .map_err(|e| corrupt(12, e))?,
        expires_at: expires
            .map(|v| v.parse::<DateTime<Utc>>())
            .transpose()
            .map_err(|e| corrupt(13, e))?,
        superseded_by: r.get(14)?,
    })
}

const GUIDE_COLS: &str = "id,scope,name,content,owner,source_failure,rationale,status,
    enforcement_layer,sensor_ref,version,added_at,last_validated_at,expires_at,superseded_by";

pub fn put_guide_rule(conn: &Connection, rule: &GuideRule) -> rusqlite::Result<GuideRule> {
    let existing: Option<(u32, String)> = conn
        .query_row(
            "SELECT version + 1, added_at FROM _amux_harness_rules WHERE id=?1",
            [&rule.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let mut stored = rule.clone();
    if let Some((next, added_at)) = existing {
        stored.version = next;
        stored.added_at = added_at
            .parse::<DateTime<Utc>>()
            .map_err(|e| corrupt(1, e))?;
    } else {
        stored.version = 1;
    }
    conn.execute(
        &format!(
            "INSERT INTO _amux_harness_rules ({GUIDE_COLS})
                  VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
                  ON CONFLICT(id) DO UPDATE SET scope=?2,name=?3,content=?4,owner=?5,
                  source_failure=?6,rationale=?7,status=?8,enforcement_layer=?9,sensor_ref=?10,
                  version=?11,last_validated_at=?13,expires_at=?14,superseded_by=?15"
        ),
        params![
            stored.id,
            stored.scope,
            stored.name,
            stored.content,
            stored.owner,
            stored.source_failure,
            stored.rationale,
            token(stored.status),
            token(stored.enforcement_layer),
            stored.sensor_ref,
            stored.version,
            stored.added_at.to_rfc3339(),
            stored.last_validated_at.map(|v| v.to_rfc3339()),
            stored.expires_at.map(|v| v.to_rfc3339()),
            stored.superseded_by,
        ],
    )?;
    Ok(stored)
}

pub fn list_guide_rules(conn: &Connection) -> rusqlite::Result<Vec<GuideRule>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {GUIDE_COLS} FROM _amux_harness_rules ORDER BY scope,name"
    ))?;
    let rows = stmt.query_map([], guide_from_row)?.collect();
    rows
}

/// Stable hash of the active guide/sensor configuration attached to context
/// snapshots and verification records.
pub fn harness_version(conn: &Connection) -> rusqlite::Result<String> {
    let rules = list_guide_rules(conn)?;
    let sensors = list_sensor_profiles(conn)?;
    let mut hash = Sha256::new();
    for rule in rules
        .iter()
        .filter(|r| matches!(r.status, GuideRuleStatus::Active))
    {
        hash.update(rule.id.as_bytes());
        hash.update(rule.version.to_be_bytes());
        hash.update(rule.content.as_bytes());
        hash.update(token(rule.enforcement_layer).as_bytes());
    }
    for sensor in sensors {
        hash.update(sensor.task_type.as_bytes());
        hash.update(sensor.version.to_be_bytes());
        hash.update(json(&sensor.criteria)?.as_bytes());
    }
    Ok(hex::encode(&hash.finalize()[..8]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use amux_core::harness::EnforcementLayer;

    #[test]
    fn checkpoint_versions_and_harness_hash_move_only_with_authoritative_state() {
        let conn = crate::db::migrate::test_memdb();
        let checkpoint = TaskCheckpoint {
            task_id: "AMUX-1".into(),
            version: 0,
            completed_steps: vec!["built".into()],
            next_action: "Run the focused test".into(),
            artifacts: vec!["commit:abc".into()],
            unresolved: vec![],
            input_hash: "input-1".into(),
            context_hash: None,
            last_result: None,
            updated_by: "worker-a".into(),
            updated_at: Utc::now(),
        };
        assert_eq!(put_checkpoint(&conn, &checkpoint).unwrap().version, 1);
        assert_eq!(put_checkpoint(&conn, &checkpoint).unwrap().version, 2);
        assert_eq!(get_checkpoint(&conn, "AMUX-1").unwrap().unwrap().version, 2);

        let empty = harness_version(&conn).unwrap();
        let rule = GuideRule {
            id: "rule-1".into(),
            scope: "global".into(),
            name: "run-tests".into(),
            content: "run tests".into(),
            owner: "platform".into(),
            source_failure: "INC-1".into(),
            rationale: "green claim without a test".into(),
            status: GuideRuleStatus::Active,
            enforcement_layer: EnforcementLayer::Guide,
            sensor_ref: None,
            version: 0,
            added_at: Utc::now(),
            last_validated_at: None,
            expires_at: None,
            superseded_by: None,
        };
        let first = put_guide_rule(&conn, &rule).unwrap();
        assert_ne!(empty, harness_version(&conn).unwrap());

        let mut revised = rule;
        revised.content = "run focused and full tests".into();
        revised.added_at = Utc::now() + chrono::Duration::days(1);
        let second = put_guide_rule(&conn, &revised).unwrap();
        assert_eq!(second.version, 2);
        assert_eq!(
            second.added_at, first.added_at,
            "updates preserve creation time"
        );
    }
}
