//! Instruction messages shaped for the chat templates of local engines.
//!
//! Model chat templates often accept one system message, only at the start, and have no
//! developer role, while clients such as Codex send several instruction messages, some of them
//! mid-conversation. Developer and system messages carry the same authority, so the leading ones
//! become one system message and later ones become user messages in place, keeping their order.

use dotcore_proto::dotcore::inference::v1::{
    ChatMessage, ChatMessageRole, chat_content_part, chat_message,
};

use crate::inference::InvalidRequest;

/// Folds the instruction messages of a chat history into the shape model templates accept.
///
/// The leading run of system and developer messages becomes one system message, joined by blank
/// lines when there are several, which then must hold only text. A later system or developer
/// message becomes a user message with the same content.
pub(super) fn fold(messages: Vec<ChatMessage>) -> Result<Vec<ChatMessage>, InvalidRequest> {
    let leading = messages
        .iter()
        .take_while(|message| is_instruction(message))
        .count();
    let mut messages = messages.into_iter();
    let mut folded = Vec::with_capacity(messages.len());
    let mut instructions = messages.by_ref().take(leading).collect::<Vec<_>>();
    if instructions.len() == 1 {
        let mut instruction = instructions.remove(0);
        instruction.role = ChatMessageRole::System as i32;
        folded.push(instruction);
    } else if !instructions.is_empty() {
        let texts = instructions
            .iter()
            .map(instruction_text)
            .collect::<Result<Vec<_>, _>>()?;
        folded.push(ChatMessage {
            role: ChatMessageRole::System as i32,
            content: Some(chat_message::Content::TextContent(texts.join("\n\n"))),
            ..Default::default()
        });
    }
    folded.extend(messages.map(|mut message| {
        if is_instruction(&message) {
            message.role = ChatMessageRole::User as i32;
        }
        message
    }));
    Ok(folded)
}

fn is_instruction(message: &ChatMessage) -> bool {
    message.role == ChatMessageRole::System as i32
        || message.role == ChatMessageRole::Developer as i32
}

/// Returns an instruction message's text, its parts joined in order; an image cannot be merged.
fn instruction_text(message: &ChatMessage) -> Result<String, InvalidRequest> {
    let mut text = match &message.content {
        Some(chat_message::Content::TextContent(content)) => content.clone(),
        Some(chat_message::Content::NullContent(_)) | None => String::new(),
        Some(chat_message::Content::ToolResult(_)) => {
            return Err(InvalidRequest::invalid_field(Some("messages")));
        }
    };
    for part in &message.content_parts {
        match &part.content {
            Some(chat_content_part::Content::TextContent(content)) => text.push_str(content),
            Some(chat_content_part::Content::ImageContent(_)) | None => {
                return Err(InvalidRequest::invalid_field(Some("messages")));
            }
        }
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::inference::v1::{ChatContentPart, ChatImageContent};

    use super::*;

    #[test]
    fn codex_history_gets_one_leading_system_message_and_later_instructions_as_user() {
        let history = vec![
            text(ChatMessageRole::System, "You are Codex."),
            parts(ChatMessageRole::Developer, &["Sandbox: ", "read-only."]),
            parts(ChatMessageRole::Developer, &["Skills: none."]),
            parts(ChatMessageRole::User, &["hi"]),
            ChatMessage {
                reasoning: Some("Say hello.".to_owned()),
                ..text(ChatMessageRole::Assistant, "hello")
            },
            parts(ChatMessageRole::Developer, &["Sandbox: workspace-write."]),
            text(ChatMessageRole::System, "Be brief."),
            parts(ChatMessageRole::User, &["again"]),
        ];

        assert_eq!(
            fold(history).expect("text instructions"),
            [
                text(
                    ChatMessageRole::System,
                    "You are Codex.\n\nSandbox: read-only.\n\nSkills: none."
                ),
                parts(ChatMessageRole::User, &["hi"]),
                ChatMessage {
                    reasoning: Some("Say hello.".to_owned()),
                    ..text(ChatMessageRole::Assistant, "hello")
                },
                parts(ChatMessageRole::User, &["Sandbox: workspace-write."]),
                text(ChatMessageRole::User, "Be brief."),
                parts(ChatMessageRole::User, &["again"]),
            ]
        );
    }

    #[test]
    fn a_single_leading_instruction_only_becomes_system() {
        let developer = ChatMessage {
            name: Some("ops".to_owned()),
            ..parts(ChatMessageRole::Developer, &["Rules."])
        };

        assert_eq!(
            fold(vec![developer.clone(), text(ChatMessageRole::User, "hi")])
                .expect("one instruction"),
            [
                ChatMessage {
                    role: ChatMessageRole::System as i32,
                    ..developer
                },
                text(ChatMessageRole::User, "hi"),
            ]
        );
        assert_eq!(
            fold(vec![
                text(ChatMessageRole::User, "hi"),
                text(ChatMessageRole::Developer, "Rules.")
            ])
            .expect("no leading instruction"),
            [
                text(ChatMessageRole::User, "hi"),
                text(ChatMessageRole::User, "Rules.")
            ]
        );
    }

    #[test]
    fn leading_instructions_with_an_image_cannot_merge() {
        let image = ChatMessage {
            role: ChatMessageRole::Developer as i32,
            content_parts: vec![ChatContentPart {
                content: Some(chat_content_part::Content::ImageContent(
                    ChatImageContent::default(),
                )),
            }],
            ..Default::default()
        };

        assert_eq!(
            fold(vec![text(ChatMessageRole::System, "You are Codex."), image]),
            Err(InvalidRequest::invalid_field(Some("messages")))
        );
    }

    fn text(role: ChatMessageRole, content: &str) -> ChatMessage {
        ChatMessage {
            role: role as i32,
            content: Some(chat_message::Content::TextContent(content.to_owned())),
            ..Default::default()
        }
    }

    fn parts(role: ChatMessageRole, texts: &[&str]) -> ChatMessage {
        ChatMessage {
            role: role as i32,
            content_parts: texts
                .iter()
                .map(|text| ChatContentPart {
                    content: Some(chat_content_part::Content::TextContent((*text).to_owned())),
                })
                .collect(),
            ..Default::default()
        }
    }
}
