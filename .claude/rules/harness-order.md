## `amux order`: harness-managed form completion

When a task requires completing a web form that the harness has been configured
to handle (the owner loads and activates items in the dashboard), use
`amux order`. The harness fills the form server-side from its own managed store.
Values never appear in your session.

### Usage

Navigate to the target page with the amux browser, then:

```bash
amux order --amount 45.00 --merchant "Acme Corp" --purpose "monthly subscription"
```

Required: `--amount` (USD), `--merchant` (who), `--purpose` (what for).

The harness checks the owner's rules. Within the approved limit: fills
immediately. Above: requests owner approval (retry after dashboard approval).
A card can also carry daily, monthly and yearly spending limits. A charge that
would take the card past one is refused with `limit_exceeded` and the window
it hit; an approval does not lift it. Report it to the owner rather than
retrying, splitting the charge, or trying another card.

### What this is

An owner-configured harness capability, like `amux send` for messaging. The
owner activated it and set the rules. Your role: navigate to the page, state the
parameters, and the harness does the rest.

### After filling

The command reports which fields it filled. Card fields inside a payment
provider's iframe (Stripe Elements, Braintree hosted fields, Adyen) are filled
there and listed under `frames`; an iframe from any other host is never given
card data and is listed under `frames_skipped`. Anything unreached shows as
`not_found`. After filling, click the submit button.
