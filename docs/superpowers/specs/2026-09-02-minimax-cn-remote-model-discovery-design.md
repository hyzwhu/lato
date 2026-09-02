# MiniMax CN Remote Model Discovery Reliability Design

## Problem

During interactive provider setup, `minimax-cn` fetches its model list from the remote provider catalog. A transient empty or non-JSON response currently produces a low-level JSON parse error and then silently falls back to the built-in `MiniMax-M2.1` entry. That behavior is misleading because the credential appears to have failed, and it violates the requirement that the remote catalog be authoritative.

## Requirements

- The model choices for `minimax-cn` must come exclusively from the current remote catalog response.
- Transient network failures, empty bodies, and malformed JSON responses must be retried with a bounded policy.
- If all attempts fail, interactive setup must report a clear error and stop before model selection.
- Neither built-in models nor a previously cached remote response may be used after a failed refresh.
- Error diagnostics must never expose credentials or authorization headers.
- Other providers retain their existing discovery and fallback behavior unless they use the same strict remote-catalog policy explicitly.

## Design

Introduce an explicit strict remote-discovery policy for `minimax-cn` at the CLI configuration boundary. The remote catalog client remains responsible for HTTP requests, response decoding, and catalog parsing. It will support bounded retries for transport errors, empty response bodies, and JSON decoding failures. Successful HTTP responses are read as bytes before decoding so errors can distinguish an empty response from malformed JSON and report the HTTP status plus a short, sanitized response summary.

Interactive configuration will recognize `minimax-cn` as strict. On successful discovery, it will present only the models returned by the remote catalog. It will not merge the built-in catalog entry into the choices. On terminal failure, it will return the discovery error immediately and will not print the model-selection prompt. The existing remote model store may still be updated after a successful response for normal runtime lookup, but it will not satisfy a failed interactive refresh.

Strict discovery must not read or parse the existing model store before making the remote request. A malformed derived cache is not an authoritative input and therefore cannot block remote discovery. After a successful remote response, the store update will acquire the normal store lock and attempt to load the existing document. If that document is malformed, it will be moved to a timestamped sibling with a `.corrupt-<timestamp>` suffix before a fresh document containing the remote result is written atomically. This preserves evidence for recovery while restoring a usable cache. Credential files are separate and must never be moved or rewritten by this recovery path.

The retry policy will use a small fixed attempt budget with short bounded backoff so setup remains responsive. HTTP authentication is not involved in the `pi.dev` catalog request, and retries must reuse only a freshly constructed catalog request without logging credential state.

## Error Handling

Errors will identify the provider and failure category: transport failure, unsuccessful HTTP status, empty body, malformed JSON, or invalid catalog shape. Malformed-response diagnostics will include only a short sanitized prefix of the response body. After the attempt budget is exhausted, the error will state that remote model discovery failed and setup was stopped. No built-in or cached fallback message will be emitted for `minimax-cn`.

A malformed local model cache is handled only after a successful authoritative response. If preserving the corrupt cache or writing the replacement fails, setup stops with a local cache recovery error rather than presenting models that cannot be resolved later. The corrupt backup remains recoverable and contains only derived model metadata, never provider credentials.

## Testing

Unit and integration coverage will verify:

- a valid remote object-shaped catalog yields only its remote models;
- an empty or malformed first response is retried and a later valid response succeeds;
- repeated malformed responses return an error after the bounded attempt count;
- strict `minimax-cn` configuration never merges or falls back to `MiniMax-M2.1`;
- terminal discovery failure prevents the model-selection stage;
- a NUL-filled or otherwise malformed model store does not prevent the remote request;
- a successful remote response preserves the malformed store as a timestamped corrupt backup and installs a valid replacement;
- diagnostics do not contain the configured API key.

## Scope

This change is limited to reliable, authoritative remote model discovery for `minimax-cn`. It does not change credential storage, MiniMax inference requests, or the discovery semantics of unrelated providers.
