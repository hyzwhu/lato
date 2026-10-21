---
name: customer-report
description: Look up a customer and generate a report using the business demo modules.
argument-hint: Customer name, for example Acme
---

The requested customer is: $ARGUMENTS

1. Discover the business-demo MCP tools if their qualified names are not yet known.
2. Call lookup_customer for the requested customer. If it is missing, ask which
   configured customer the user wants; do not invent customer records.
3. Pass the returned customer ID to create_customer_report.
4. Confirm the report only after that tool succeeds. Return its exact artifact
   path and the verified revenue/order metrics. Explain that the data is synthetic.

Use the business tools for these operations. Do not replace them with guessed
results or shell/file operations.
