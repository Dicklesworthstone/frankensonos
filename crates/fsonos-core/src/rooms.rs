//! Room resolution: from what a person or agent types to the players a
//! command must address, across both households.
//!
//! Accepted forms, tried in order:
//! 1. a room name, exact first, then case-insensitive with curly apostrophes
//!    folded (`ada's studio` matches `Ada’s Studio`);
//! 2. a household-qualified room, `Name@S1` / `Name@S2` / `Name@<household id>`,
//!    which disambiguates a name both households use;
//! 3. a player id (`RINCON_…`, case-insensitive), resolving to that player's room;
//! 4. a reserved target: `all` / `everywhere` (every room, or `all@S1` for one
//!    household) and `here` (the calling client's default room);
//! 5. an owner-defined alias from `aliases.toml` ([`Aliases`]), naming one
//!    room or a set;
//! 6. a unique prefix of at least three characters (`kit` for `Kitchen`).
//!
//! Near misses (small edit distances) never act: they only feed the
//! suggestions in errors ([`suggest_rooms`]). Errors carry what an agent needs
//! to retry: the known rooms, or the qualified candidates for an ambiguous name.

mod aliases;

pub use aliases::{Alias, AliasError, AliasWarning, Aliases};

use crate::{CoreError, HouseholdState, Room};
use fsonos_types::{Generation, Player};

/// Words that name targets rather than rooms; an alias cannot use them.
pub const RESERVED: [&str; 3] = ["all", "everywhere", "here"];

/// Prefixes shorter than this never act (too likely to be a typo).
pub const MIN_PREFIX: usize = 3;

/// Normalize a room name for matching: trim, lowercase, and fold curly
/// apostrophes to a straight one (Sonos room names commonly use U+2019).
#[must_use]
pub fn normalize_room(name: &str) -> String {
    name.trim()
        .chars()
        .flat_map(|c| match c {
            '\u{2019}' | '\u{2018}' | '\u{201B}' => '\''.to_lowercase(),
            other => other.to_lowercase(),
        })
        .collect()
}

/// Everything a command for one room needs.
#[derive(Debug, Clone, Copy)]
pub struct ControlTarget<'a> {
    pub household: &'a HouseholdState,
    pub room: &'a Room,
    /// The room's primary player: the address for room-level commands.
    pub player: &'a Player,
    /// The coordinator of the room's group: the address for group-wide
    /// commands (transport, queue, group volume).
    pub coordinator: &'a Player,
}

/// Who is asking, and the owner's aliases.
#[derive(Debug, Clone, Copy, Default)]
pub struct ResolveContext<'a> {
    pub aliases: Option<&'a Aliases>,
    /// The calling client's identity (e.g. its Tailscale login), for `here`.
    pub client: Option<&'a str>,
}

/// How a query matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matched {
    /// A room name, `Name@Label`, or player id.
    Room,
    /// `all` / `everywhere`.
    Everywhere,
    /// `here`: the client's default room.
    Here,
    /// An `aliases.toml` entry.
    Alias,
    /// A unique prefix.
    Prefix,
}

/// What a query resolved to: one room, or (for `all` and multi-room aliases)
/// a set, each with its own household.
#[derive(Debug, Clone)]
pub struct Resolution<'a> {
    pub targets: Vec<ControlTarget<'a>>,
    pub matched: Matched,
}

/// A short, retry-able label per household, index-aligned with `households`:
/// `S1`/`S2` when that generation is unique among them, else the household
/// id, else `#<index>`.
#[must_use]
pub fn household_labels(households: &[HouseholdState]) -> Vec<String> {
    let generations: Vec<_> = households.iter().map(HouseholdState::generation).collect();
    households
        .iter()
        .zip(&generations)
        .enumerate()
        .map(|(i, (h, generation))| match generation {
            Some(g) if generations.iter().filter(|o| *o == generation).count() == 1 => {
                generation_label(*g).to_string()
            }
            _ => {
                h.id.as_ref()
                    .map_or_else(|| format!("#{i}"), |id| id.0.clone())
            }
        })
        .collect()
}

fn generation_label(g: Generation) -> &'static str {
    match g {
        Generation::S1 => "S1",
        Generation::S2 => "S2",
    }
}

/// Every room as `Name@<household label>`, for listings and error messages.
#[must_use]
pub fn known_rooms(households: &[HouseholdState]) -> Vec<String> {
    let labels = household_labels(households);
    households
        .iter()
        .zip(&labels)
        .flat_map(|(h, label)| h.rooms.iter().map(move |r| format!("{}@{label}", r.name)))
        .collect()
}

/// Up to three of `known` (each `Name@Label`) whose names are close to
/// `query`: a containment either way, or a small edit distance. Best first.
#[must_use]
pub fn suggest_rooms(query: &str, known: &[String]) -> Vec<String> {
    let bare = |s: &str| normalize_room(s.rsplit_once('@').map_or(s, |(name, _)| name));
    let wanted = bare(query);
    if wanted.is_empty() {
        return Vec::new();
    }
    let budget = (wanted.chars().count() / 3).max(2);
    let mut scored: Vec<(usize, &String)> = known
        .iter()
        .filter_map(|candidate| {
            let name = bare(candidate);
            let score = if name.contains(&wanted) || wanted.contains(&name) {
                0
            } else {
                edit_distance(&wanted, &name)
            };
            (score <= budget).then_some((score, candidate))
        })
        .collect();
    scored.sort();
    scored.into_iter().take(3).map(|(_, c)| c.clone()).collect()
}

/// Levenshtein distance over chars.
#[must_use]
pub fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitute = diagonal + usize::from(ca != *cb);
            diagonal = row[j + 1];
            row[j + 1] = substitute.min(row[j] + 1).min(diagonal + 1);
        }
    }
    row[b.len()]
}

/// Resolve `query` to exactly one room (no aliases, no client defaults). A
/// query naming several rooms is [`CoreError::AmbiguousRoom`].
pub fn resolve_room<'a>(
    households: &'a [HouseholdState],
    query: &str,
) -> Result<ControlTarget<'a>, CoreError> {
    resolve_one(households, query, ResolveContext::default())
}

/// Resolve `query` to exactly one room, for commands that address one room.
/// A set (`all`, a multi-room alias) is [`CoreError::AmbiguousRoom`] listing
/// its rooms.
pub fn resolve_one<'a>(
    households: &'a [HouseholdState],
    query: &str,
    ctx: ResolveContext<'_>,
) -> Result<ControlTarget<'a>, CoreError> {
    let mut resolution = resolve_targets(households, query, ctx)?;
    if resolution.targets.len() == 1 {
        return Ok(resolution.targets.remove(0));
    }
    let labels = household_labels(households);
    let hits: Vec<(usize, &Room)> = resolution
        .targets
        .iter()
        .map(|t| (index_of(households, t.household), t.room))
        .collect();
    Err(ambiguous(query, &hits, &labels))
}

/// Resolve `query` to the room or set of rooms it names, for commands that
/// accept sets (group, party, announce, scenes, pause/resume/stop, mute,
/// volume). Rooms of a set that cannot be addressed right now (no known
/// player address) are left out; if none can be, the first such error is
/// returned.
pub fn resolve_targets<'a>(
    households: &'a [HouseholdState],
    query: &str,
    ctx: ResolveContext<'_>,
) -> Result<Resolution<'a>, CoreError> {
    let labels = household_labels(households);
    let q = query.trim();
    let unknown = || CoreError::UnknownRoom {
        name: q.to_string(),
        known: known_rooms(households),
    };

    // 1–3: a room by name, `Name@Label`, or player id.
    if let Some(resolution) = direct(households, &labels, q)? {
        return Ok(resolution);
    }

    // 4: reserved targets.
    let (word, qualifier) = q.rsplit_once('@').map_or((q, None), |(w, l)| (w, Some(l)));
    match normalize_room(word).as_str() {
        "all" | "everywhere" => {
            let hits: Vec<(usize, &Room)> = households
                .iter()
                .enumerate()
                .filter(|(i, h)| qualifier.is_none_or(|l| qualifies(h, &labels[*i], l)))
                .flat_map(|(i, h)| h.rooms.iter().map(move |r| (i, r)))
                .collect();
            if hits.is_empty() {
                return Err(unknown());
            }
            return set(households, &hits, Matched::Everywhere);
        }
        "here" if qualifier.is_none() => {
            let default = ctx
                .aliases
                .and_then(|a| a.default_room(ctx.client))
                .ok_or_else(unknown)?;
            if RESERVED.contains(&normalize_room(default).as_str()) {
                return Err(unknown());
            }
            let mut resolution = resolve_targets(households, default, ctx)?;
            resolution.matched = Matched::Here;
            return Ok(resolution);
        }
        _ => {}
    }

    // 5: an alias, whose rooms resolve by name / `Name@Label` / id only (an
    // alias never names another alias, so there are no cycles).
    if let Some(alias) = ctx.aliases.and_then(|a| a.get(q)) {
        let mut hits: Vec<(usize, &Room)> = Vec::new();
        for room in &alias.rooms {
            for hit in direct_hits(households, &labels, room) {
                if !hits
                    .iter()
                    .any(|(i, r)| *i == hit.0 && r.primary == hit.1.primary)
                {
                    hits.push(hit);
                }
            }
        }
        if hits.is_empty() {
            return Err(unknown());
        }
        return set(households, &hits, Matched::Alias);
    }

    // 6: a unique prefix.
    let wanted = normalize_room(q);
    if wanted.chars().count() >= MIN_PREFIX {
        let hits: Vec<(usize, &Room)> = all_rooms(households)
            .filter(|(_, r)| normalize_room(&r.name).starts_with(&wanted))
            .collect();
        match hits.as_slice() {
            [] => {}
            [(i, room)] => {
                return Ok(Resolution {
                    targets: vec![target(&households[*i], room)?],
                    matched: Matched::Prefix,
                });
            }
            many => return Err(ambiguous(q, many, &labels)),
        }
    }

    Err(unknown())
}

/// Steps 1–3 as one room: `Ok(None)` when nothing matched, an error when
/// several rooms did.
fn direct<'a>(
    households: &'a [HouseholdState],
    labels: &[String],
    q: &str,
) -> Result<Option<Resolution<'a>>, CoreError> {
    match direct_hits(households, labels, q).as_slice() {
        [] => Ok(None),
        [(i, room)] => Ok(Some(Resolution {
            targets: vec![target(&households[*i], room)?],
            matched: Matched::Room,
        })),
        many => Err(ambiguous(q, many, labels)),
    }
}

fn direct_hits<'a>(
    households: &'a [HouseholdState],
    labels: &[String],
    q: &str,
) -> Vec<(usize, &'a Room)> {
    let q = q.trim();
    let exact: Vec<_> = all_rooms(households).filter(|(_, r)| r.name == q).collect();
    if !exact.is_empty() {
        return exact;
    }
    let wanted = normalize_room(q);
    let named: Vec<_> = all_rooms(households)
        .filter(|(_, r)| normalize_room(&r.name) == wanted)
        .collect();
    if !named.is_empty() {
        return named;
    }
    if let Some((name, qualifier)) = q.rsplit_once('@') {
        let wanted = normalize_room(name);
        let qualified: Vec<_> = all_rooms(households)
            .filter(|(i, r)| {
                qualifies(&households[*i], &labels[*i], qualifier)
                    && normalize_room(&r.name) == wanted
            })
            .collect();
        if !qualified.is_empty() {
            return qualified;
        }
    }
    all_rooms(households)
        .filter(|(_, r)| r.players.iter().any(|p| p.0.eq_ignore_ascii_case(q)))
        .collect()
}

fn all_rooms(households: &[HouseholdState]) -> impl Iterator<Item = (usize, &Room)> {
    households
        .iter()
        .enumerate()
        .flat_map(|(i, h)| h.rooms.iter().map(move |r| (i, r)))
}

fn qualifies(h: &HouseholdState, label: &str, qualifier: &str) -> bool {
    let qualifier = qualifier.trim();
    label.eq_ignore_ascii_case(qualifier)
        || h.id
            .as_ref()
            .is_some_and(|id| id.0.eq_ignore_ascii_case(qualifier))
        || h.generation()
            .is_some_and(|g| generation_label(g).eq_ignore_ascii_case(qualifier))
}

fn index_of(households: &[HouseholdState], household: &HouseholdState) -> usize {
    households
        .iter()
        .position(|h| std::ptr::eq(h, household))
        .expect("a target's household comes from the slice it was resolved in")
}

fn set<'a>(
    households: &'a [HouseholdState],
    hits: &[(usize, &'a Room)],
    matched: Matched,
) -> Result<Resolution<'a>, CoreError> {
    let mut targets = Vec::new();
    let mut first_error = None;
    for (i, room) in hits {
        match target(&households[*i], room) {
            Ok(t) => targets.push(t),
            Err(e) => {
                first_error.get_or_insert(e);
            }
        }
    }
    match (targets.is_empty(), first_error) {
        (true, Some(e)) => Err(e),
        _ => Ok(Resolution { targets, matched }),
    }
}

fn ambiguous(query: &str, hits: &[(usize, &Room)], labels: &[String]) -> CoreError {
    let qualified: Vec<String> = hits
        .iter()
        .map(|(i, r)| format!("{}@{}", r.name, labels[*i]))
        .collect();
    // A qualifier only helps when it is unique; otherwise offer the id.
    let candidates = hits
        .iter()
        .zip(&qualified)
        .map(|((_, r), q)| {
            if qualified.iter().filter(|o| *o == q).count() == 1 {
                q.clone()
            } else {
                r.primary.0.clone()
            }
        })
        .collect();
    CoreError::AmbiguousRoom {
        name: query.trim().to_string(),
        candidates,
    }
}

fn target<'a>(
    household: &'a HouseholdState,
    room: &'a Room,
) -> Result<ControlTarget<'a>, CoreError> {
    let find = |id: &fsonos_types::PlayerId| {
        household
            .player(id)
            .ok_or_else(|| CoreError::UnknownPlayer(id.0.clone()))
    };
    Ok(ControlTarget {
        household,
        room,
        player: find(&room.primary)?,
        coordinator: find(&room.coordinator)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_types::PlayerId;

    #[test]
    fn normalizes_curly_apostrophe_and_case() {
        assert_eq!(normalize_room("  Ada\u{2019}s Studio "), "ada's studio");
        assert_eq!(normalize_room("ÉTUDE"), "étude");
    }

    #[test]
    fn labels_fall_back_to_id_then_index() {
        let a = HouseholdState {
            id: Some(fsonos_types::HouseholdId("HH_A".into())),
            ..Default::default()
        };
        let b = HouseholdState::default();
        assert_eq!(household_labels(&[a, b]), ["HH_A", "#1"]);
    }

    #[test]
    fn duplicate_names_in_one_household_offer_player_ids() {
        let room = |id: &str| Room {
            name: "Office".into(),
            primary: PlayerId(id.into()),
            players: vec![PlayerId(id.into())],
            missing: Vec::new(),
            coordinator: PlayerId(id.into()),
        };
        let player = |id: &str| Player {
            id: PlayerId(id.into()),
            room_name: "Office".into(),
            ip: "192.0.2.1".parse().unwrap(),
            model: String::new(),
            generation: Generation::S2,
        };
        let st = HouseholdState {
            players: vec![player("RINCON_A"), player("RINCON_B")],
            rooms: vec![room("RINCON_A"), room("RINCON_B")],
            ..Default::default()
        };
        let houses = [st];
        match resolve_room(&houses, "office") {
            Err(CoreError::AmbiguousRoom { candidates, .. }) => {
                assert_eq!(candidates, ["RINCON_A", "RINCON_B"]);
            }
            other => panic!("expected AmbiguousRoom, got {other:?}"),
        }
        assert_eq!(
            resolve_room(&houses, "rincon_b").unwrap().player.id.0,
            "RINCON_B"
        );
    }

    #[test]
    fn unknown_room_lists_known_rooms() {
        let err = resolve_room(&[], "Garage").unwrap_err();
        assert_eq!(
            err.to_string(),
            "unknown room \"Garage\"; known rooms: (none discovered yet)"
        );
    }

    #[test]
    fn suggestions_rank_close_names_only() {
        let known: Vec<String> = ["Kitchen@S1", "Den@S1", "Ada\u{2019}s Studio@S1", "Patio@S2"]
            .map(String::from)
            .into();
        assert_eq!(suggest_rooms("Kitchn", &known), ["Kitchen@S1"]);
        assert_eq!(suggest_rooms("studio", &known), ["Ada\u{2019}s Studio@S1"]);
        assert_eq!(suggest_rooms("patio@S1", &known), ["Patio@S2"]);
        assert_eq!(suggest_rooms("Garage", &known).len(), 0);
        assert_eq!(suggest_rooms("  ", &known).len(), 0);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("", "den"), 3);
    }
}
