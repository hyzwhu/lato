// Install in $LATO_HOME/workflows/customer-report.rhai.
// args.customer must be supplied by the caller; no customer is guessed.
let meta = #{ name: "customer-report", description: "Look up a customer and create a verified business report" };
if !args.contains("customer") || args.customer == "" {
    throw "customer is required";
}
phase("Look up customer");
let lookup = agent("LOOKUP_PHASE: Use the business-demo lookup_customer module for customer "
    + args.customer + ". Return the exact customer ID, revenue and currency from its result.",
    #{ capability_mode: "read-write" });
if !lookup.success { throw "Customer lookup failed"; }
phase("Generate report");
let report = agent("REPORT_PHASE: Use the business-demo create_customer_report module with the verified customer ID below. Confirm only after success and return the exact artifact path. Data is synthetic. Verified lookup: "
    + json_encode(lookup.output), #{ capability_mode: "read-write" });
if !report.success { throw "Report generation failed"; }
complete(#{customer:args.customer, lookup:lookup.output, report:report.output});
