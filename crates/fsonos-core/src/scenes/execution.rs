//! Confirm grouping before dependent playback. Never replay a group write
//! whose reply was lost; topology is the authority for whether it happened.

use super::{
    ApplyOptions, HouseholdState, Room, SceneOp, ScenePlan, SceneReport, fade_volumes, leads, run,
    silence_starting,
};
use fsonos_proto::Transport;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_types::PlayerId;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

const SETTLE: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(100);
const BLOCKED: &str = "not sent: the scene's required group membership was not confirmed";

pub(super) fn apply<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    plan: &ScenePlan,
    options: &ApplyOptions<'_>,
) -> SceneReport {
    let mut live = households.to_vec();
    let mut dependencies = Dependencies::new(households, &plan.ops);
    let mut report = SceneReport {
        notes: plan.notes.clone(),
        ..SceneReport::default()
    };
    let mut favorites = HashMap::new();
    for op in plan.ops.iter().filter(|op| is_grouping(op)) {
        if dependencies.blocked(op) {
            report.failed.push((op.clone(), BLOCKED.into()));
            continue;
        }
        let before = live.clone();
        let written = run(t, &live, op, options, &mut favorites);
        match confirm(t, &before, &mut live, op) {
            Ok(()) => {
                if written.is_err() {
                    report.notes.push(
                        "a group change was confirmed by topology after its reply failed; not repeated"
                            .into(),
                    );
                }
                report.done.push(op.clone());
            }
            Err(observation) => {
                dependencies.block(op);
                let detail = written.err().map_or_else(
                    || observation.clone(),
                    |error| format!("{error}; {observation}"),
                );
                report.failed.push((op.clone(), detail));
            }
        }
    }
    // A successful individual move is not enough if an unexpected room
    // joined meanwhile. Check the entire intended audience before any sound.
    for op in plan.ops.iter().filter(|op| !is_grouping(op)) {
        if let Some(coordinator) = sound_target(op)
            && dependencies.changed(op)
            && !audience_matches(households, &live, plan, coordinator)
        {
            dependencies.block(op);
        }
    }
    let ready = ScenePlan {
        ops: plan
            .ops
            .iter()
            .filter(|op| !is_grouping(op))
            .filter_map(|op| {
                if dependencies.blocked(op) {
                    report.failed.push((op.clone(), BLOCKED.into()));
                    None
                } else {
                    Some(op.clone())
                }
            })
            .collect(),
        notes: Vec::new(),
    };
    // Only confirmed groups may be silenced for a fade. This plan contains
    // no speculative regrouping; silence_starting reads the actual layout.
    let ins = options
        .fade
        .map_or_default(|(fader, _)| silence_starting(t, &live, &ready, fader));
    let mut failed_sources = HashSet::new();
    for op in &ready.ops {
        if options.fade.is_some() && matches!(op, SceneOp::Volume { .. }) {
            continue;
        }
        if sound_target(op).is_some_and(|c| failed_sources.contains(c)) {
            report.failed.push((
                op.clone(),
                "not sent: the group's requested source did not start".into(),
            ));
            continue;
        }
        match run(t, &live, op, options, &mut favorites) {
            Ok(true) => report.done.push(op.clone()),
            Ok(false) => {}
            Err(error) => {
                if let Some(coordinator) = sound_target(op) {
                    failed_sources.insert(coordinator.clone());
                }
                report.failed.push((op.clone(), error));
            }
        }
    }
    if let Some(fade) = options.fade {
        fade_volumes(t, &live, &ready, &ins, fade, &mut report);
    }
    report
}

fn is_grouping(op: &SceneOp) -> bool {
    matches!(op, SceneOp::Leave { .. } | SceneOp::Join { .. })
}

fn sound_target(op: &SceneOp) -> Option<&PlayerId> {
    match op {
        SceneOp::Favorite { coordinator, .. }
        | SceneOp::Uri { coordinator, .. }
        | SceneOp::Queue { coordinator, .. }
        | SceneOp::QueueBackup { coordinator, .. }
        | SceneOp::Dj { coordinator, .. }
        | SceneOp::Play { coordinator }
        | SceneOp::Pause { coordinator } => Some(coordinator),
        _ => None,
    }
}

fn target(op: &SceneOp) -> &PlayerId {
    match op {
        SceneOp::Leave { player, .. }
        | SceneOp::Join { player, .. }
        | SceneOp::Volume { player, .. }
        | SceneOp::Mute { player, .. } => player,
        SceneOp::Favorite { coordinator, .. }
        | SceneOp::Uri { coordinator, .. }
        | SceneOp::Queue { coordinator, .. }
        | SceneOp::QueueBackup { coordinator, .. }
        | SceneOp::Dj { coordinator, .. }
        | SceneOp::Play { coordinator }
        | SceneOp::Pause { coordinator } => coordinator,
    }
}

fn room<'a>(households: &'a [HouseholdState], player: &PlayerId) -> Option<&'a Room> {
    households
        .iter()
        .flat_map(|h| &h.rooms)
        .find(|r| r.players.contains(player))
}

fn observed(before: &[HouseholdState], live: &[HouseholdState], op: &SceneOp) -> bool {
    let Some(original) = room(before, target(op)) else {
        return false;
    };
    let Some(current) = room(live, &original.primary) else {
        return false;
    };
    // A bonded room must move as one unit, not abandon a hidden player.
    if !original.players.iter().all(|p| current.players.contains(p)) {
        return false;
    }
    match op {
        SceneOp::Leave { .. } => {
            leads(current)
                && live.iter().flat_map(|h| &h.rooms).all(|other| {
                    other.primary == current.primary || other.coordinator != current.coordinator
                })
        }
        SceneOp::Join { coordinator, .. } => room(live, coordinator)
            .is_some_and(|lead| leads(lead) && lead.players.contains(&current.coordinator)),
        _ => false,
    }
}

/// Retry bounded reads, never writes. Prefer the receiving coordinator or
/// old coordinator as a witness: a leaving player's own topology can lag.
/// A witness that fails is not asked repeatedly during this transition.
fn confirm<T: Transport + ?Sized>(
    t: &T,
    before: &[HouseholdState],
    live: &mut [HouseholdState],
    op: &SceneOp,
) -> Result<(), String> {
    let player = target(op);
    let (index, household) = before
        .iter()
        .enumerate()
        .find(|(_, h)| h.player(player).is_some())
        .ok_or_else(|| "group player is no longer addressable".to_string())?;
    let old = room(before, player).map(|r| &r.coordinator);
    let mut ids = Vec::new();
    if let SceneOp::Join { coordinator, .. } = op {
        ids.push(coordinator);
    }
    ids.extend(old);
    ids.push(player);
    let mut hosts = Vec::new();
    for id in ids {
        if let Some(p) = household.player(id)
            && !hosts.contains(&p.ip)
        {
            hosts.push(p.ip);
        }
    }
    let started = Instant::now();
    let mut detail = "group change was not observed before the scene's settle deadline".to_string();
    loop {
        let mut failed = Vec::new();
        for host in &hosts {
            match get_zone_group_state(t, *host) {
                Ok(topology) => {
                    live[index].apply_topology(&topology);
                    if observed(before, live, op) {
                        return Ok(());
                    }
                }
                Err(error) => {
                    detail = format!("group topology could not be confirmed: {error}");
                    failed.push(*host);
                }
            }
        }
        hosts.retain(|host| !failed.contains(host));
        let left = SETTLE.saturating_sub(started.elapsed());
        if hosts.is_empty() || left.is_zero() {
            return Err(detail);
        }
        std::thread::sleep(POLL.min(left));
    }
}

fn audience_matches(
    before: &[HouseholdState],
    live: &[HouseholdState],
    plan: &ScenePlan,
    coordinator: &PlayerId,
) -> bool {
    let (Some(original), Some(current)) = (room(before, coordinator), room(live, coordinator))
    else {
        return false;
    };
    if !leads(current) {
        return false;
    }
    let destination = |player: &PlayerId| {
        plan.ops.iter().rev().find_map(|op| match op {
            SceneOp::Leave { player: p, .. } if p == player => Some(p),
            SceneOp::Join {
                player: p,
                coordinator,
                ..
            } if p == player => Some(coordinator),
            _ => None,
        })
    };
    let mut expected: Vec<_> = before
        .iter()
        .flat_map(|h| &h.rooms)
        .filter(|r| {
            if let Some(to) = destination(&r.primary) {
                return *to == original.primary;
            }
            r.primary == original.primary
                || (leads(original)
                    && destination(&original.primary).is_none()
                    && r.coordinator == original.coordinator)
        })
        .map(|r| r.primary.0.as_str())
        .collect();
    let mut actual: Vec<_> = live
        .iter()
        .flat_map(|h| &h.rooms)
        .filter(|r| r.coordinator == current.coordinator)
        .map(|r| r.primary.0.as_str())
        .collect();
    expected.sort_unstable();
    actual.sort_unstable();
    expected == actual
}

/// Original groups plus planned joins form dependency components. A failed
/// move blocks its component, not another independent group or household.
struct Dependencies {
    rooms: Vec<Room>,
    roots: Vec<usize>,
    changed: HashSet<usize>,
    blocked: HashSet<usize>,
}

impl Dependencies {
    fn new(households: &[HouseholdState], ops: &[SceneOp]) -> Self {
        let rooms: Vec<_> = households.iter().flat_map(|h| h.rooms.clone()).collect();
        let mut this = Self {
            roots: (0..rooms.len()).collect(),
            rooms,
            changed: HashSet::new(),
            blocked: HashSet::new(),
        };
        for i in 0..this.rooms.len() {
            for j in 0..i {
                if this.rooms[i].coordinator == this.rooms[j].coordinator {
                    this.union(i, j);
                }
            }
        }
        for op in ops {
            if let SceneOp::Join {
                player,
                coordinator,
                ..
            } = op
                && let (Some(a), Some(b)) = (this.index(player), this.index(coordinator))
            {
                this.union(a, b);
            }
        }
        for op in ops.iter().filter(|op| is_grouping(op)) {
            if let Some(root) = this.root(op) {
                this.changed.insert(root);
            }
        }
        this
    }

    fn index(&self, player: &PlayerId) -> Option<usize> {
        self.rooms.iter().position(|r| r.players.contains(player))
    }

    fn union(&mut self, a: usize, b: usize) {
        let (keep, replace) = (self.roots[a], self.roots[b]);
        for root in &mut self.roots {
            if *root == replace {
                *root = keep;
            }
        }
    }

    fn root(&self, op: &SceneOp) -> Option<usize> {
        self.index(target(op)).map(|i| self.roots[i])
    }

    fn changed(&self, op: &SceneOp) -> bool {
        self.root(op).is_some_and(|r| self.changed.contains(&r))
    }

    fn blocked(&self, op: &SceneOp) -> bool {
        self.root(op).is_some_and(|r| self.blocked.contains(&r))
    }

    fn block(&mut self, op: &SceneOp) {
        if let Some(root) = self.root(op) {
            self.blocked.insert(root);
        }
    }
}
