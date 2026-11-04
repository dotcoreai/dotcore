//! Chooses the inference route a coding agent starts with, among the routes the inference API
//! lists: the one the operator named, else the one picked on the terminal.
//!
//! A named route must be listed. Without a name the operator picks on the controlling terminal;
//! without a terminal the command asks for a name instead.

use super::{CodeError, CodingAgent};
use crate::{
    management::single_line,
    terminal::{SelectItem, SelectPrompt, Terminal, TerminalCondition},
};

/// Finds the listed route named `name` for `agent`.
pub(super) fn find_route<'a>(
    agent: CodingAgent,
    routes: &'a [String],
    name: &str,
) -> Result<&'a str, CodeError> {
    require_routes(routes)?;
    routes
        .iter()
        .find(|route| *route == name)
        .map(String::as_str)
        .ok_or_else(|| CodeError::UnknownRoute {
            agent,
            name: name.to_owned(),
            ready: route_names(routes),
        })
}

/// Asks the operator to pick a route on `terminal`.
pub(super) fn pick_route<'a>(
    agent: CodingAgent,
    routes: &'a [String],
    terminal: &mut dyn Terminal,
) -> anyhow::Result<&'a str> {
    require_routes(routes)?;
    if terminal.condition() != TerminalCondition::Interactive {
        return Err(CodeError::RouteRequired {
            agent,
            ready: route_names(routes),
        }
        .into());
    }

    let items = routes
        .iter()
        .map(|route| SelectItem::new(single_line(route)));
    let prompt = SelectPrompt::new(format!("Route for {}", agent.display_name()), items);
    Ok(&routes[terminal.select(&prompt)?])
}

fn require_routes(routes: &[String]) -> Result<(), CodeError> {
    if routes.is_empty() {
        return Err(CodeError::NoRoute);
    }
    Ok(())
}

fn route_names(routes: &[String]) -> Vec<String> {
    routes.iter().map(|route| single_line(route)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{ScriptedAnswer, ScriptedPrompt, ScriptedTerminal};

    fn routes() -> Vec<String> {
        ["draft", "chat", "code"].map(str::to_owned).to_vec()
    }

    #[test]
    fn a_named_route_must_be_listed() {
        let routes = routes();

        assert_eq!(
            find_route(CodingAgent::Opencode, &routes, "chat").ok(),
            Some("chat")
        );
        assert_eq!(
            find_route(CodingAgent::Opencode, &routes, "Chat")
                .expect_err("names match exactly")
                .to_string(),
            "no ready inference route is named \"Chat\"; ready routes: draft, chat, code"
        );
    }

    #[test]
    fn no_listed_route_fails_before_naming_or_picking() {
        let mut terminal = ScriptedTerminal::interactive([]);

        assert!(matches!(
            find_route(CodingAgent::Opencode, &[], "chat"),
            Err(CodeError::NoRoute)
        ));
        let error = pick_route(CodingAgent::Opencode, &[], &mut terminal).expect_err("no route");
        assert!(matches!(
            error.downcast_ref::<CodeError>(),
            Some(CodeError::NoRoute)
        ));
        assert_eq!(terminal.prompts(), []);
    }

    #[test]
    fn the_picker_lists_every_route() {
        let routes = routes();
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Select(1)]);

        let picked = pick_route(CodingAgent::Opencode, &routes, &mut terminal).expect("picked");

        assert_eq!(picked, "chat");
        assert_eq!(
            terminal.prompts(),
            [ScriptedPrompt::Select(SelectPrompt::new(
                "Route for OpenCode",
                [
                    SelectItem::new("draft"),
                    SelectItem::new("chat"),
                    SelectItem::new("code"),
                ],
            ))]
        );
    }

    #[test]
    fn picking_without_a_terminal_asks_for_a_route_name() {
        let routes = routes();
        let mut terminal = ScriptedTerminal::new(TerminalCondition::NonInteractive, []);

        let error =
            pick_route(CodingAgent::Opencode, &routes, &mut terminal).expect_err("no terminal");

        assert_eq!(
            error.to_string(),
            "choosing an inference route needs a terminal; pass --model with one of: draft, \
             chat, code"
        );
        assert_eq!(terminal.prompts(), []);
    }
}
