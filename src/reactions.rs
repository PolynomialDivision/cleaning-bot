//! What reactions mean — as plain state changes, so the Matrix handlers in
//! `main` only feed events in, then save and refresh the messages. (Group
//! selector taps themselves are `onboarding::tap`.)
//!
//! A ✅ on the weekly plan marks the reacting person's open duties of that
//! week done — or, when their shift is still to come, swaps them onto the
//! running one (`trades`). Two kinds of reactions can be taken back:
//!   * a group selector tap (`onboarding`): its owner undoes it, as long as
//!     that still applies;
//!   * a ✅ on the weekly plan: whoever reacted takes their done mark back —
//!     unless the turn has been marked done anew since.
//!
//! Every redaction is remembered (`State::redacted_reactions`), so a
//! reaction that only arrives after its redaction — or again, after a
//! restart — doesn't count. The bot's own redactions (it clears consumed
//! taps, see `onboarding::consume_tap`) never get here.

use mxbot_common::matrix_sdk::ruma::RoomId;

use crate::{
    onboarding,
    state::{MarkedDuty, ReactionDone, State},
    BotContext,
};

/// What a ✅ on a weekly plan did.
#[derive(Debug, PartialEq)]
pub enum PlanDone {
    /// Counted before, or already taken back: nothing changed.
    Seen,
    /// They have no open duty in that week; nothing changed.
    NothingOpen,
    /// Their open duties were marked done.
    Marked,
    /// Their own shift was still to come, so they took the running one
    /// (marked done) and its holder theirs — see `trades`.
    SwappedEarly(crate::state::Trade),
}

/// `sender` reacted ✅ (`reaction_id`) on the plan of `week`: mark their
/// open duties of that week done — those that have started, or the next
/// one, as `!done` would.
pub fn plan_done(
    state: &mut State,
    reaction_id: &str,
    sender: &str,
    week: (i32, u32),
) -> anyhow::Result<PlanDone> {
    if state.reaction_dones.contains_key(reaction_id)
        || state.redacted_reactions.contains(reaction_id)
    {
        return Ok(PlanDone::Seen);
    }
    state.apply_event(crate::analytics::DomainEvent::PersonCreated {
        person_id: uuid::Uuid::new_v4().to_string(),
        display_name: sender.into(),
        matrix_id: Some(sender.into()),
    })?;
    let person_id = state
        .person_by_matrix_id(sender)
        .map(|p| p.id.clone())
        .ok_or_else(|| anyhow::anyhow!("{sender} has no person record"))?;
    let mut duties = crate::commands::markable_duties(state, &person_id, week, None);
    let mut swapped = None;
    if duties.is_empty() {
        let today = crate::state::today();
        match crate::trades::swap_early(state, &person_id, sender, week, None, today)? {
            Some((trade, cleaned)) => {
                duties = cleaned;
                swapped = Some(trade);
            }
            None => return Ok(PlanDone::NothingOpen),
        }
    } else {
        crate::commands::mark_duties_done(state, &person_id, &duties)?;
    }
    let group_id = duties[0].group.id.clone();
    // When each mark was made, so taking this reaction back later can't
    // undo a newer mark of the same turn.
    let times = duties
        .iter()
        .filter_map(|d| {
            state
                .completion_for(&d.group, d.slot_index, d.turn)
                .map(|c| c.completed_at)
        })
        .collect();
    state
        .reaction_completion_times
        .insert(reaction_id.to_owned(), times);
    state.reaction_dones.insert(
        reaction_id.to_owned(),
        ReactionDone {
            group_id,
            completed_by_id: person_id,
            iso_year: week.0,
            iso_week: week.1,
            marked: duties
                .iter()
                .map(|d| MarkedDuty {
                    group_id: d.group.id.clone(),
                    slot_id: d.group.slots.get(d.slot_index).map(|s| s.id.clone()),
                    shift: d.turn.shift,
                })
                .collect(),
        },
    );
    Ok(swapped.map_or(PlanDone::Marked, PlanDone::SwappedEarly))
}

/// What a redaction did.
#[derive(Debug, PartialEq)]
pub enum Redaction {
    /// Someone else's tap or ✅ — not theirs to take back. Nothing changed.
    Refused,
    /// Nothing to undo (any longer); remembered against late delivery.
    Noted,
    /// A selector tap taken back: its group membership was undone.
    TapUndone,
    /// A ✅ taken back: done marks of this week were removed.
    DoneUndone { year: i32, week: u32 },
}

/// `sender` redacted `redacted_id` in `room_id`.
pub fn redaction(
    ctx: &BotContext,
    state: &mut State,
    room_id: &RoomId,
    sender: &str,
    redacted_id: &str,
) -> Redaction {
    // Only the one who tapped can take a tap back…
    if state
        .group_selectors
        .values()
        .find(|s| s.taps.contains_key(redacted_id))
        .is_some_and(|s| s.user_id != sender || !onboarding::in_room(s, room_id, ctx))
    {
        return Redaction::Refused;
    }
    // …and only who reacted ✅ (on the plan, in the cleaning room) can take
    // that back.
    if state.reaction_dones.get(redacted_id).is_some_and(|rd| {
        room_id != ctx.room_id
            || state
                .person_by_id(&rd.completed_by_id)
                .and_then(|p| p.matrix_id.as_deref())
                != Some(sender)
    }) {
        return Redaction::Refused;
    }
    state.redacted_reactions.insert(redacted_id.to_owned());

    match onboarding::untap(ctx, state, redacted_id) {
        Ok(Some(_)) => return Redaction::TapUndone,
        Ok(None) => {}
        Err(e) => tracing::error!("Undoing a group selector tap failed: {e}"),
    }

    let Some(mut rd) = state.reaction_dones.remove(redacted_id) else {
        return Redaction::Noted;
    };
    // Keep only the marks this very reaction made: if a turn was undone and
    // marked done again since, the new mark isn't this reaction's to take.
    if let Some(times) = state.reaction_completion_times.remove(redacted_id) {
        rd.marked.retain(|d| {
            state.completions.iter().any(|c| {
                c.group_id == d.group_id
                    && c.slot_id == d.slot_id
                    && c.shift == d.shift
                    && (c.iso_year, c.iso_week) == (rd.iso_year, rd.iso_week)
                    && times.contains(&c.completed_at)
            })
        });
        if rd.marked.is_empty() {
            return Redaction::Noted;
        }
    }
    match crate::commands::undo_reaction_done(state, &rd) {
        Ok(true) => Redaction::DoneUndone {
            year: rd.iso_year,
            week: rd.iso_week,
        },
        Ok(false) => Redaction::Noted,
        Err(e) => {
            tracing::error!("Undoing a ✅ reaction failed: {e}");
            Redaction::Noted
        }
    }
}
