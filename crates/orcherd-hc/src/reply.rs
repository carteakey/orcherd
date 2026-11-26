//! Typing a reply to an agent in a herdr-core terminal. `terminal.send`
//! types into whatever program has the terminal, a shell included, so the
//! reply is checked against what the core says now, just before it goes,
//! and an idle agent's is typed only while the core still reports it idle.

use crate::{
    Core, Error, Result,
    agent::Until,
    wire::{STATUS_MISMATCH, SendText, StatusState},
};

pub use crate::identity::agent_in_front;

/// What became of a reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replied {
    Sent,
    /// The agent it was written to no longer runs in the terminal.
    Gone,
}

/// Blocking: types `text` into `terminal` for the agent named `agent`, if
/// that agent still has it. The core refuses a blocked agent
/// ([`crate::wire::BLOCKED`]), and an idle one that went to work meanwhile
/// ([`crate::wire::STATUS_MISMATCH`]).
pub fn reply(core: &Core, terminal: &str, agent: &str, text: &str) -> Result<Replied> {
    let listed = core.list(false)?;
    let Some(record) = listed
        .terminals
        .iter()
        .find(|record| record.address == terminal)
    else {
        return Ok(Replied::Gone);
    };
    if !agent_in_front(record, agent) {
        return Ok(Replied::Gone);
    }
    let idle = record
        .status
        .as_ref()
        .is_some_and(|status| status.state == StatusState::Idle);
    core.send(&SendText {
        terminal,
        text,
        only_if: idle.then_some("idle"),
        ..SendText::default()
    })?;
    Ok(Replied::Sent)
}

/// Blocking: types `text` into `terminal` for the agent named `agent`
/// once its turn is over, as notes the user wrote reach a Herdr 1 agent:
/// nothing joins a running turn until `until`'s deadline, after which the
/// text goes in anyway. The core refuses a blocked agent
/// ([`crate::wire::BLOCKED`]); a stop request is `Error::Stopped`.
pub fn when_free(
    core: &Core,
    terminal: &str,
    agent: &str,
    text: &str,
    until: Until<'_>,
) -> Result<Replied> {
    loop {
        let listed = core.list(false)?;
        let Some(record) = listed
            .terminals
            .iter()
            .find(|record| record.address == terminal)
        else {
            return Ok(Replied::Gone);
        };
        if !agent_in_front(record, agent) {
            return Ok(Replied::Gone);
        }
        let state = record.status.as_ref().map(|status| status.state);
        let left = until.check_in()?;
        if state == Some(StatusState::Working)
            && let Some(timeout) = left
        {
            core.wait(terminal, timeout)?;
            continue;
        }
        // Past the deadline the text goes in whatever the agent does.
        let idle = left.is_some() && state == Some(StatusState::Idle);
        let sent = core.send(&SendText {
            terminal,
            text,
            only_if: idle.then_some("idle"),
            ..SendText::default()
        });
        match sent {
            Ok(_) => return Ok(Replied::Sent),
            // It went to work between the list and the send: wait again.
            Err(Error::Refused {
                kind: Some(kind), ..
            }) if kind == STATUS_MISMATCH => continue,
            Err(error) => return Err(error),
        }
    }
}
