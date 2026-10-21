# Installed Lato CLI: real MiniMax verification

Date: 2026-10-21 (Asia/Shanghai)

Result: passed. These checks used the installed executable
`/Users/huangyongzhao/.cargo/bin/lato` (`0.1.0-beta.2`) and real MiniMax HTTPS
inference, not a scripted provider or `FakeModelStream`.

Requested model: `MiniMax-M3.1-Flash`, using the official China OpenAI-compatible
endpoint `https://api.minimax.cn/v1`. The model ID and endpoint were checked against
[MiniMax's official documentation](https://platform.minimax.cn/docs/api-reference/text-openai-api).
A temporary `models.json` selected this model through `minimax-live/MiniMax-M3.1-Flash`.

| Check | Result | Elapsed | Evidence |
|---|---|---|---|
| `lato -p --model minimax-live/MiniMax-M3.1-Flash` handshake | Passed | 3.36 s | Received `LATO_REAL_OK` from the real model |
| `lato -p` natural-language customer lookup and report generation | Passed | 17.08 s | Actual MCP lookup and report execution; generated report verified |
| Installed `lato serve`, explicit `business-demo:customer-report` Skill | Passed | 33.16 s | Two tool approvals; completed result; actual generated report verified |
| Installed `lato serve`, natural-language request to start `customer-report` Workflow | Passed | 31.42 s | Exact revision and budget 2; two child agents; four approvals; run `complete`; generated report verified |

The model independently chose the business tool calls. In the Workflow test, the
first child looked up Acme and returned customer ID `acme`, revenue `42000 CNY` and
12 orders. The second child used that verified customer ID to generate the report.
The pending approvals exposed distinct child execution session IDs under the
owning parent session. The final run retained both business outputs and consumed
two logical agents within its budget. The main response correctly said the run
was started; the test separately polled the background run to completion.

The CLI invocation used a temporary Lato home, a copied demo plugin and
`--sandbox workspace`. The service used `--trust-workspace` and explicit per-call
HTTP approvals. Each generated artifact was removed before the next business
scenario, so one scenario could not pass using a previous scenario's file.

The artifact contained the actual synthetic fixture metrics, and model replies
identified the records as demonstration data. See
[the generated report](2026-10-21-minimax-acme-report.md) and
[structured results including model replies and approval arguments](2026-10-21-minimax-live.json).
The original temporary artifact path in the model reply was checked during the
run and cleaned afterward; the copied report above remains available for review.

The supplied credential was entered through a hidden prompt and passed to child
processes through an environment variable. It was not saved in repository files,
reports or the user's persistent Lato credential store. Temporary model/home
configuration and test processes were removed after the checks.

Scope: a live happy-path check of three business invocation paths with one model,
not a reliability benchmark. Business data and modules are synthetic local
fixtures; no real customer system was accessed. No implementation changes were
needed during this live test.
