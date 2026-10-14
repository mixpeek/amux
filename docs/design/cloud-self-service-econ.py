"""Unit economics for amux Cloud (docs/design/cloud-self-service.md, section 4).
Inputs are either measured (Hetzner pricing API 2026-10-14; Stripe list
prices; PostHog and GitHub traffic 2026-10-14) or marked H (hypothesis).
Run: python3 docs/design/cloud-self-service-econ.py"""
import math
COST = {'solo': 9.99 + 0.60 + 2.00, 'team': 18.49 + 0.60 + 3.70, 'max': 34.99 + 0.60 + 7.00}   # server + IPv4 + backups
MIX = {'solo': 0.80, 'team': 0.16, 'max': 0.04}            # H; EU only at launch (US deferred)
LADDERS = {'$26 (2x cost)': {'solo': 26, 'team': 46, 'max': 86},
           '$39 entry':     {'solo': 39, 'team': 69, 'max': 129},
           '$49 entry':     {'solo': 49, 'team': 89, 'max': 159}}
EU_CONSUMER = 0.35     # H: share of customers who are EU consumers (pay VAT; prices shown VAT-inclusive)
VAT = 0.21             # H: average EU VAT rate
INTL = 0.75            # H: share of cards issued outside the US (EU default product)
REFUND, ABUSE, DISPUTE_RATE, DISPUTE_FEE = 0.01, 0.005, 0.003, 15.0   # H, H, H, Stripe list fee per dispute
SUPPORT_TOKENS = 0.40  # H: support-agent inference per customer per month
LAPSED_SNAPSHOT = 0.02 * 1.0   # H: 2% of customers lapsed at any time x ~$1 snapshot each (servers deleted, not stopped)
# R2 per customer: litestream sync every 60 s = 43,200 PUTs/month at $4.50/M, plus a nightly restic snapshot
# (~1,000 PUTs) and ~3 GB stored at $0.015/GB-month (H: measure PUTs per server in the synthetic run)
R2 = 44200 / 1e6 * 4.50 + 3 * 0.015
PAYMENTS = {
  'Stripe': dict(pct=0.029 + 0.007 + 0.005 + INTL * (0.015 + 0.01), fix=0.30, disputes=True),   # card, Billing, Tax, intl + FX
}
SYNTHETIC_RUNS_PER_DAY = 4                             # every 6 hours and on every deploy (section 12)
def fixed(n, operator=True):
    control = 7.09 * max(1, math.ceil(n / 1500))      # H: one cx23 control-plane node per 1500 servers
    synthetic = (0.0137 + 0.05) * SYNTHETIC_RUNS_PER_DAY * 30   # one started cx33 hour + ~$0.05 tokens per run (H)
    clerk = 0 if n <= 10000 else 0.02 * (n - 10000)    # H: check the current Clerk tier
    vat_filing = 50.0                                  # H: EU OSS quarterly returns through Stripe's tax-filing integration, per month
    if not operator:                                   # path to 10 runs without the Operator (section 13)
        return control + synthetic + clerk + vat_filing
    operator_server = 12.59                            # the Operator's own amux server (cx33)
    return control + synthetic + clerk + operator_server + vat_filing
def operator_tokens(contribution_before):              # cap: $50 + 5% of contribution, at most $250 (section 4.7)
    return min(250.0, 50 + 0.05 * max(0.0, contribution_before))
OWNER_RATE = 150                                       # H: owner time at $/hour
def owner_hours(n): return n / 1000                    # target: no routine work; ~1 h of exceptions per 1000 customers (H)
def month(n, ladder, pay, operator=True):
    p = PAYMENTS[pay]; rev = net = var = 0.0
    for plan, share in MIX.items():
        c = n * share; price = LADDERS[ladder][plan]
        gross = c * price
        net_price = price * (1 - EU_CONSUMER) + price / (1 + VAT) * EU_CONSUMER   # VAT leaves with the tax authority
        rev += gross; net += c * net_price
        var += c * (COST[plan] + price * (p['pct'] + REFUND + ABUSE) + p['fix'] + SUPPORT_TOKENS + LAPSED_SNAPSHOT + R2
                    + (DISPUTE_RATE * DISPUTE_FEE if p['disputes'] else 0))
    f = fixed(n, operator)
    if operator:
        f += operator_tokens(net - var - f)
    contrib = net - var - f
    return rev, net, var, f, contrib, contrib - owner_hours(n) * OWNER_RATE

# Owner interventions (section 4.8). Per 100 customers per month: (count, minutes each) [H].
INTERVENTIONS = {
    'optimistic':  {'support the agent could not resolve': (1.0, 15), 'billing exception': (0.2, 15), 'infrastructure beyond auto-recovery': (0.2, 20),
                    'abuse decision': (0.1, 20), 'third-party data or legal request': (0.05, 30)},
    'realistic':   {'support the agent could not resolve': (2.0, 20), 'billing exception': (0.5, 20), 'infrastructure beyond auto-recovery': (0.5, 30),
                    'abuse decision': (0.2, 30), 'third-party data or legal request': (0.1, 30)},
    'pessimistic': {'support the agent could not resolve': (5.0, 30), 'billing exception': (1.0, 30), 'infrastructure beyond auto-recovery': (1.5, 45),
                    'abuse decision': (0.5, 45), 'third-party data or legal request': (0.3, 45)},
}
FIXED_OWNER_MIN = {'optimistic': 40, 'realistic': 75, 'pessimistic': 150}   # report reading, provider changes, renewals, App Store
OWNER_ONLY = {'abuse decision', 'third-party data or legal request'}         # never outsourced (L3)
CONTRACTOR_RATE, CONTRACTOR_MIN_MIN, CONTRACTOR_RETAINER = 60.0, 15, 300.0  # H: ops/support contractor
def owner_load(n, scen):
    var_min = sum(cnt * n / 100 * mins for cnt, mins in INTERVENTIONS[scen].values())
    only_min = sum(cnt * n / 100 * mins for k, (cnt, mins) in INTERVENTIONS[scen].items() if k in OWNER_ONLY)
    out_min = sum(cnt * n / 100 * max(mins, CONTRACTOR_MIN_MIN) for k, (cnt, mins) in INTERVENTIONS[scen].items() if k not in OWNER_ONLY)
    return var_min + FIXED_OWNER_MIN[scen], only_min + FIXED_OWNER_MIN[scen], CONTRACTOR_RETAINER + out_min / 60 * CONTRACTOR_RATE

# Sample size for a two-sided two-proportion test, alpha 0.05, power 0.80 (section 10, M14).
def n_per_arm(p1, rel_lift, z_a=1.959964, z_b=0.841621):
    p2 = p1 * (1 + rel_lift)
    return (z_a + z_b) ** 2 * (p1 * (1 - p1) + p2 * (1 - p2)) / (p2 - p1) ** 2

if __name__ == '__main__':
    print('Monthly at steady state, USD. "Net" is after VAT.')
    print(f"{'payments':28} {'ladder':14} {'N':>5} {'gross':>9} {'net':>9} {'variable':>9} {'fixed':>6} {'contrib':>9} {'margin':>7} {'after owner':>11}")
    for pay in PAYMENTS:
        for L in LADDERS:
            for n in (100, 500, 1000, 5000):
                g, nt, v, f, c, co = month(n, L, pay)
                print(f"{pay:28} {L:14} {n:5d} {g:9,.0f} {nt:9,.0f} {v:9,.0f} {f:6,.0f} {c:9,.0f} {c/nt:7.0%} {co:11,.0f}")
    print('\nPrice and churn: per 1,000 buyers at the $26 conversion rate (H: $39 converts 0.80x, $49 0.65x), Stripe')
    CONV = {'$26 (2x cost)': 1.0, '$39 entry': 0.80, '$49 entry': 0.65}
    for L in LADDERS:
        unit = (month(1000, L, 'Stripe')[4] + fixed(1000)) / 1000
        for churn in (0.05, 0.12):
            n0 = 1000 * CONV[L]; alive = [n0 * (1 - churn) ** m for m in range(3)]
            print(f"  {L:14} churn {churn:4.0%}: per-customer contrib ${unit:5.2f}, day-90 customers {alive[2]*(1-churn):5.0f}, months 1-3 ${sum(a*unit for a in alive):9,.0f}, lifetime ${n0*unit/churn:10,.0f}")
    print('\nVolume (measured 2026-10-14: amux.io 3,596 unique visitors/30d, 178 viewed /pricing; GitHub 459 uniques/14d)')
    for pay_rate in (0.01, 0.02, 0.05, 0.08):  # H: share of pricing-page viewers who buy
        for growth in (0.0, 0.10):              # H: monthly growth of pricing-page viewers
            n, m, viewers = 0.0, 0, 178.0
            while n < 100 and m < 60:
                n = n * (1 - 0.05) + viewers * pay_rate; viewers *= 1 + growth; m += 1
            print(f"  {pay_rate:4.0%} of pricing viewers buy, viewers grow {growth:4.0%}/month: 100 customers in {m if n >= 100 else '60+'} months")
        ceiling = 178 * pay_rate / 0.05
        print(f"    flat-traffic ceiling at 5% churn: {ceiling:4.0f} customers, about ${month(max(1,round(ceiling)), '$39 entry', 'Stripe')[4]:,.0f}/month contribution at $39")
    print('\nOwner interventions at $39 on Stripe (section 4.8). Owner time at $150/h; contractor $60/h, 15-min minimum, $300/month retainer')
    print(f"  {'scenario':12} {'N':>5} {'contrib':>8} {'owner h (all)':>13} {'after owner time':>16} {'owner h (owner-only)':>20} {'contractor $':>12} {'after outsourcing':>17}")
    for scen in INTERVENTIONS:
        for n in (10, 100, 500, 1000):
            c = month(n, '$39 entry', 'Stripe', operator=(n >= 100))[4]
            all_min, only_min, contractor = owner_load(n, scen)
            print(f"  {scen:12} {n:5d} {c:8,.0f} {all_min/60:13.1f} {c - all_min/60*OWNER_RATE:16,.0f} {only_min/60:20.1f} {contractor:12,.0f} {c - contractor - only_min/60*OWNER_RATE:17,.0f}")
    print('\nPath to 10 (no Operator) at $39 on Stripe:')
    for n in (1, 3, 5, 10):
        r = month(n, '$39 entry', 'Stripe', operator=False)
        print(f"  {n:3d} customers: contribution ${r[4]:7,.0f}/month (fixed ${r[3]:,.0f})")
    print('\nA/B sample sizes (two-sided, alpha 0.05, power 0.80) at measured traffic: 3,596 visits and 178 pricing views a month')
    for label, base, monthly in (('visit -> pricing view', 178/3596, 3596), ('pricing view -> paid', 0.02, 178)):
        for lift in (0.2, 0.5, 1.0):
            n = n_per_arm(base, lift)
            print(f"  {label:22} base {base:5.1%} lift +{lift:4.0%}: {n:8,.0f} per arm = {2*n/monthly:6.1f} months of traffic")
    print(f"\nR2 per customer per month: ${R2:.2f} (sync every 60 s); at the 1 s default it would be up to ${2.6e6/1e6*4.5:.2f}")
    BUILD_AGENT_DAYS, AGENT_DAY_COST = 32, 100.0     # H: path-to-10 steps 1-8; inference per agent-day
    for n in (10, 100):
        c = month(n, '$39 entry', 'Stripe', operator=(n >= 100))[4]
        print(f"  build ~{BUILD_AGENT_DAYS} agent-days x ${AGENT_DAY_COST:.0f} = ${BUILD_AGENT_DAYS*AGENT_DAY_COST:,.0f}; payback at {n} customers: {BUILD_AGENT_DAYS*AGENT_DAY_COST/c:.1f} months")
