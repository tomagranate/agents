# ChatGPT export format for the archive importer

Research date: 2026-08-09

## Conclusion

The format is stable enough for a first importer. OpenAI documents the export package, but not its inner JSON schema. Therefore, the importer must use permissive decoding and strict output rules.

OpenAI exports a ZIP file with chat history and other account data. The chat data is in `conversations.json`. Large exports can use numbered conversation JSON files instead. [OpenAI export guide](https://help.openai.com/en/articles/7260999-exporting-your-chatgpt-history-and-data), [OpenAI transfer guide](https://help.openai.com/en/articles/9106926-transferring-conversations-from-1-chatgpt-account-to-another-chatgpt-account)

Current public parsers also report nested ZIP files and names such as `conversations-001.json`. Treat this as compatibility evidence, not an OpenAI contract. [OpenAI Export Parser format notes](https://github.com/temnoon/openai_export_parser/blob/b0985df2887dfd0df4668b2b7815d8e6627f637c/README.md#L200-L216)

## Stable JSON shape

Each conversation file normally contains a JSON array. Each array item is one conversation. A real-export-derived schema and a sanitized public sample both show this root shape. [Inferred schema](https://gist.github.com/dmarx/08afeb669cdc2f974d6aca61dcce360d), [sanitized export sample](https://github.com/terminalcommandnewsletter/everything-chatgpt/blob/main/sample/conversations.json)

Use this partial Rust model. Keep every field optional except the mapping default.

```rust
struct Conversation {
    id: Option<String>,
    conversation_id: Option<String>,
    title: Option<String>,
    create_time: Option<f64>,
    update_time: Option<f64>,
    current_node: Option<String>,
    default_model_slug: Option<String>,
    mapping: BTreeMap<String, Node>,
}

struct Node {
    id: Option<String>,
    message: Option<Message>,
    parent: Option<String>,
    children: Vec<String>,
}

struct Message {
    id: Option<String>,
    author: Option<Author>,
    create_time: Option<f64>,
    update_time: Option<f64>,
    content: serde_json::Value,
    metadata: serde_json::Value,
    channel: Option<String>,
}
```

The conversation object commonly has these fields:

| Field | Meaning | Import rule |
|---|---|---|
| `conversation_id`, `id` | Conversation identity | Prefer `conversation_id`; use `id` for older exports. |
| `title` | Display title | Use an empty or fixed fallback title when absent. |
| `create_time`, `update_time` | Unix seconds | Accept integer or fractional numbers and null. Convert in UTC. |
| `mapping` | Nodes keyed by node ID | Default to an empty map. |
| `current_node` | Selected leaf node | Use it to select the visible path. |
| `default_model_slug` | Conversation model hint | Use only when a message has no model. |
| `is_archived` | ChatGPT archive state | Optional metadata. It is not message content. |

Other observed fields include `moderation_results`, `plugin_ids`, `gizmo_id`, `gizmo_type`, `conversation_template_id`, `safe_urls`, and `blocked_urls`. Ignore these by default. The inferred schema shows these fields and the node structure. [Inferred schema, node and conversation fields](https://gist.github.com/dmarx/08afeb669cdc2f974d6aca61dcce360d#file-conversations-json-schema-js)

## Graph and branch rules

`mapping` is a message graph. Each node has an ID, one nullable parent, child IDs, and a nullable message. The null-message root stub is normal.

Use `current_node` as the selected leaf. Follow each `parent` link to the root. Reverse the result to get display order. This keeps the active branch and excludes edited or regenerated siblings. A current parser uses this exact method. [Active-path traversal](https://github.com/slyubarskiy/chatgpt-conversation-extractor/blob/b7c4372b518a006df57415b0d4287fbbdf88ed29/src/chatgpt_extractor/extractor.py#L634-L705)

Do not sort all mapping messages by time. That merges active and abandoned branches.

Add a visited-node set. Stop on a cycle, missing node, or missing parent. If `current_node` is absent or invalid, choose a deterministic message leaf. Prefer the newest leaf, then use its node ID as a tie breaker. Emit a diagnostic for this fallback.

The current archive event model has no message IDs or parent IDs. Version one should import only the active path. A later schema can retain all branches with `native_message_id` and `parent_message_id`.

## Messages, roles, and content

Observed roles include `user`, `assistant`, `system`, and `tool`. Treat the role as an open string during decoding. Keep only visible `user` and `assistant` messages in the normalized archive.

Messages commonly contain `id`, `author`, timestamps, `content`, `metadata`, `recipient`, and `channel`. Recent exports can omit `recipient` and metadata. [Current parser compatibility handling](https://github.com/knu/chatgpt2obsidian/blob/dda0877313c89d03c0966622c56e7575b81a2036/chatgpt2obsidian#L645-L688)

Use these content rules:

| `content_type` | Observed shape | Version-one rule |
|---|---|---|
| `text` | `parts` array | Join non-empty string parts. |
| `multimodal_text` | Strings and object parts | Join only string parts. Ignore asset objects. |
| `code` | Usually `text` and `language` | Skip by default. Standard answer code is normally in text markdown. |
| `execution_output` | Usually `text` | Skip as tool output. |
| `user_editable_context`, `model_editable_context` | Instruction or context text | Skip. |
| `thoughts`, `reasoning_recap` | Internal process content | Skip. |
| Other types | Polymorphic object | Skip and report the type. Never serialize the raw object. |

Multimodal object parts can hold image, audio, video, transcript, and code-interpreter data. Public parsers confirm mixed string and object parts. [Content handlers](https://github.com/slyubarskiy/chatgpt-conversation-extractor/blob/b7c4372b518a006df57415b0d4287fbbdf88ed29/src/chatgpt_extractor/processors.py#L97-L288)

Skip a message when `metadata.is_visually_hidden_from_conversation` is true. Also skip empty assistant placeholders. [Filtering rules](https://github.com/slyubarskiy/chatgpt-conversation-extractor/blob/b7c4372b518a006df57415b0d4287fbbdf88ed29/src/chatgpt_extractor/processors.py#L20-L69)

## Model and time rules

Timestamps are nullable Unix seconds. Decode them as `f64`. Preserve fractional seconds when possible. Reject non-finite values and out-of-range dates.

Use `message.metadata.model_slug` for each retained event. Fall back to `conversation.default_model_slug`. Add all retained model values to the session model set. Older conversations can omit the conversation model, and messages can use different models. A parser corpus covering 2023 through 2026 confirms this behavior. [Model metadata findings](https://github.com/slyubarskiy/chatgpt-conversation-extractor/blob/b7c4372b518a006df57415b0d4287fbbdf88ed29/src/chatgpt_extractor/gpt_metadata.py#L1-L27)

Set the provider to `openai` and the source to `chatgpt`.

## Privacy boundary

Use an output allowlist. Do not copy raw conversation, message, content, or metadata objects into the archive.

Keep only these values:

- Conversation ID, title, and timestamps.
- Visible user and assistant text.
- Message timestamp and model slug.
- Selected branch node IDs, only if a future schema supports them.

Exclude these values:

- System, developer, and tool messages.
- Hidden messages, reasoning, and commentary.
- Custom instructions and memory context.
- Tool inputs, outputs, widget state, and code execution records.
- Moderation data, request IDs, and internal status values.
- Attachment bodies, asset pointers, filenames, and generated media.
- Citation and search metadata. These fields can contain full URLs.
- Other export files, including account, settings, feedback, and profile data.

This boundary matches the archive rule that forbids credentials, raw tool output, reasoning, and binary artifacts.

## Required tests

Use synthetic fixtures until the requested export arrives:

1. A null root, one user message, and one assistant message.
2. Two sibling branches with `current_node` selecting only one branch.
3. Multiple text parts and mixed multimodal parts.
4. Hidden, system, tool, context, and reasoning messages.
5. Null and fractional timestamps.
6. Per-message model precedence and conversation model fallback.
7. Missing mapping, missing node, cycle, and invalid `current_node`.
8. Unknown roles, content types, fields, and multimodal part types.
9. One base file and multiple numbered conversation files.
10. A ZIP with unsafe paths and unrelated account files.

The real ChatGPT export should become a local validation corpus. Do not check it into the repository.
