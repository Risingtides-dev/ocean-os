# Niche Taxonomy & Creator Analytics — Complete Documentation Index

**System Purpose:** Rising Tides creator niche classification, performance measurement, crossover strategy, and sound trend analytics

**Audience:** Campaign managers, data analysts, engineers, creator coaches

**Date:** June 2026

---

## Document Map

### 1. **NICHE_TAXONOMY_AND_ANALYTICS.md** (Foundation Strategy)
**What:** Comprehensive niche taxonomy with tier structure, CPM benchmarks, engagement rates, and scoring formulas

**Key Contents:**
- 10 primary niche categories (Music, Dance, Finance, Comedy, Fitness, Lifestyle, Gaming, Tech, Education, Health)
- CPM benchmarks and engagement rate expectations for each niche
- Niche Fit Score formula (50/20/15/10/5 weighting)
- Creator Momentum Score formula
- High-success crossover pairs (Music+Dance, Fitness+Music, Finance+Education)
- Risky crossovers to avoid (Comedy+Finance, Gaming+Finance)
- Niche health indicator matrix (rising vs declining signals)
- Quarterly health dashboard examples

**When to Use:** Strategic planning, portfolio rebalancing, niche recruitment strategy

**Owner:** Campaign Operations

---

### 2. **NICHE_TAXONOMY_IMPLEMENTATION.md** (Technical Playbook)
**What:** Database design, scoring engine code, integration patterns, and testing

**Key Contents:**
- PostgreSQL schema extensions (creator metrics, performance history, sound trend tracking)
- Python implementation of Niche Fit Score calculation
- Python implementation of Creator Momentum Score calculation
- Crossover detection algorithm with pattern analysis
- Sound trend adoption lag tracking system
- Niche health metrics aggregation queries
- SQL query patterns for creator matching and ranking
- Unit test examples for scoring engines

**When to Use:** Engineering implementation, database design reviews, API development

**Owner:** Engineering + Analytics

---

### 3. **SOUND_TRENDS_AND_CROSSOVER_RESEARCH.md** (Research & Operations)
**What:** Deep empirical analysis of sound trend mechanics, creator crossover patterns, and campaign deployment strategies

**Key Contents:**
- Sound trend lifecycle (days 1-30+ with CPM and engagement data)
- Four trend types: Dopamine Hits, Format-Driven, Evergreen Audio, Nanogenre Explosions
- Trend impact on creator velocity and niche performance
- Adoption speed by niche (fastest: Dance, Music, Comedy; slowest: Finance, Health)
- Detailed crossover analysis: Music+Dance (synergistic), Fitness+Music (moderate), Lifestyle+Tech (risky), Finance+Education (strong), Comedy+Finance (dangerous)
- Weekly sound trend monitoring protocol
- Campaign deployment strategies with timelines (Dopamine Hit: 48-hour response, Format-Driven: 4-week rollout)
- Quarterly case study: Dance niche health driven by sound trends
- Trend research template and adoption tracking

**When to Use:** Campaign planning, trend monitoring operations, creator coaching, weekly briefings

**Owner:** Campaign Strategy + Analytics Team

---

### 4. **NICHE_TAXONOMY_QUICK_REFERENCE.md** (Daily Operations)
**What:** One-page lookup tables, decision trees, checklists, and assessment matrices

**Key Contents:**
- CPM tier lookup by niche ($0.02-0.20 range)
- Engagement rate benchmarks and expected ranges
- Decision trees:
  - Classify creator's primary niche
  - Identify crossover potential
  - Match creators to campaigns
- Crossover pair matrix (encourage/monitor/caution/avoid)
- Momentum score quick assessment (without calculations)
- Sound trend decision matrix (adoption window, CPM lift)
- Creator tier assignments by niche
- Pre-campaign matching checklist
- FAQ section

**When to Use:** Weekly creator management, campaign briefings, quick lookups, creator onboarding

**Owner:** Campaign Operations + Creator Coaches

---

## Quick Navigation

### By Role

**Campaign Manager:**
→ Start: NICHE_TAXONOMY_QUICK_REFERENCE (decision trees, checklists)
→ Deep dive: NICHE_TAXONOMY_AND_ANALYTICS (strategy), SOUND_TRENDS_AND_CROSSOVER_RESEARCH (trend operations)

**Data Analyst:**
→ Start: NICHE_TAXONOMY_AND_ANALYTICS (metrics & formulas)
→ Deep dive: NICHE_TAXONOMY_IMPLEMENTATION (schema & queries), SOUND_TRENDS_AND_CROSSOVER_RESEARCH (empirical data)

**Engineer:**
→ Start: NICHE_TAXONOMY_IMPLEMENTATION (schema, code, queries)
→ Reference: NICHE_TAXONOMY_AND_ANALYTICS (formulas), NICHE_TAXONOMY_QUICK_REFERENCE (decision logic)

**Creator Coach:**
→ Start: NICHE_TAXONOMY_QUICK_REFERENCE (momentum assessment, crossover potential)
→ Deep dive: SOUND_TRENDS_AND_CROSSOVER_RESEARCH (trend strategies)

**Executive/Strategy:**
→ Start: NICHE_TAXONOMY_AND_ANALYTICS (executive summary, key takeaways)
→ Reference: NICHE_TAXONOMY_QUICK_REFERENCE (tier assignments)

---

### By Task

**Classify a new creator's niche:**
→ NICHE_TAXONOMY_QUICK_REFERENCE → Decision Tree 1

**Match creators to a campaign:**
→ NICHE_TAXONOMY_QUICK_REFERENCE → Decision Tree 3 (use Niche Fit Score)

**Identify crossover opportunities:**
→ NICHE_TAXONOMY_QUICK_REFERENCE → Decision Tree 2
→ SOUND_TRENDS_AND_CROSSOVER_RESEARCH → Crossover pattern analysis

**Monitor sound trend opportunities:**
→ SOUND_TRENDS_AND_CROSSOVER_RESEARCH → Weekly monitoring protocol
→ NICHE_TAXONOMY_QUICK_REFERENCE → Sound trend decision matrix

**Calculate creator Momentum Score:**
→ NICHE_TAXONOMY_IMPLEMENTATION → Creator Momentum Score Calculation section
→ NICHE_TAXONOMY_QUICK_REFERENCE → Momentum score quick assessment

**Build Niche Fit Score system:**
→ NICHE_TAXONOMY_IMPLEMENTATION → Niche Fit Score calculation + database schema

**Quarterly niche health analysis:**
→ NICHE_TAXONOMY_AND_ANALYTICS → Niche health indicators section
→ NICHE_TAXONOMY_IMPLEMENTATION → Niche health monitoring queries

**Deploy a sound trend campaign:**
→ SOUND_TRENDS_AND_CROSSOVER_RESEARCH → Campaign deployment strategies
→ NICHE_TAXONOMY_QUICK_REFERENCE → Sound trend decision matrix

---

## Key Concepts Cross-Reference

### Niche Fit Score
**Purpose:** 0-100 score for creator-to-campaign alignment

**Formula:** (Primary Match × 0.50) + (Engagement × 0.20) + (Audience Quality × 0.15) + (Trend Alignment × 0.10) + (CPM Efficiency × 0.05)

**Components:**
- Primary Match: Niche alignment (0-100)
- Engagement Strength: Creator ER vs niche average (0-100)
- Audience Quality: Demographics + authenticity + intent (0-100)
- Trend Alignment: Sound adoption lag, if applicable (0-100)
- CPM Efficiency: Creator CPM vs benchmark (0-100)

**Interpretation:**
- 85+: Tier 1 (Excellent fit)
- 70-84: Tier 2 (Good fit)
- 50-69: Tier 3 (Acceptable)
- <50: Avoid

**When to calculate:** Before each campaign matching; refresh weekly

**Where to find:**
- Strategy: NICHE_TAXONOMY_AND_ANALYTICS.md (Tier 3 section)
- Implementation: NICHE_TAXONOMY_IMPLEMENTATION.md (Scoring Engine section)
- Quick lookup: NICHE_TAXONOMY_QUICK_REFERENCE.md (Decision Tree 3)

---

### Creator Momentum Score
**Purpose:** 0-100 score indicating if creator is rising/flat/declining

**Formula:** (Follower Growth × 0.25) + (Booking Velocity × 0.25) + (Engagement Trend × 0.25) + (Sound Adoption × 0.15) + (Consistency × 0.10)

**Components:**
- Follower Growth: % growth last 90 days (0-100)
- Booking Velocity: Campaigns booked per quarter (0-100)
- Engagement Trend: ER trend up/down (0-100)
- Sound Adoption: Days from trend emergence to use (0-100)
- Consistency: Posts per week (0-100)

**Interpretation:**
- 80+: High momentum (prioritize for campaigns)
- 60-79: Moderate momentum (safe choice)
- 40-59: Low momentum (check other metrics)
- <40: Declining momentum (avoid or revive)

**When to calculate:** Weekly or before high-value campaign decisions

**Where to find:**
- Strategy: NICHE_TAXONOMY_AND_ANALYTICS.md (Creator Momentum Score section)
- Implementation: NICHE_TAXONOMY_IMPLEMENTATION.md (Momentum Score Calculation section)
- Quick lookup: NICHE_TAXONOMY_QUICK_REFERENCE.md (Momentum score quick assessment)

---

### Niche Health Indicators
**Purpose:** Quarterly assessment of niche vitality (rising/stable/declining)

**Rising Signals:**
- Sound adoption lag compressing (<7 days)
- Creator growth rate > 15% quarterly
- CPM trajectory +10% or more
- Content consistency > 70% posting 3+ times/week
- Sponsorship interest strong (>1.5 campaigns per creator/quarter)
- Trend adaptation > 60% adopting within 14 days

**Declining Signals:**
- Sound adoption lag extending (>30 days)
- Creator attrition > 25% inactive 90+ days
- CPM collapse >15% year-over-year
- Content decay (>40% posting <1 time/week)
- Sponsorship drought (<0.5 campaigns per creator/quarter)
- Algorithm resistance (views -20% vs baseline)

**When to measure:** Quarterly (end of each quarter)

**Where to find:**
- Strategy: NICHE_TAXONOMY_AND_ANALYTICS.md (Niche Health Indicators section)
- Implementation: NICHE_TAXONOMY_IMPLEMENTATION.md (Niche Health Monitoring section)
- Quick dashboard: NICHE_TAXONOMY_AND_ANALYTICS.md (Quarterly health dashboard example)

---

### Sound Trend Adoption Lag
**Purpose:** Measure how fast a creator adopts trending sounds (days from emergence to first use)

**Benchmark by niche:**
- Fastest: Dance (3-5 days), Music (4-6 days), Comedy (4-6 days)
- Medium: Fitness (5-8 days), Lifestyle (6-10 days)
- Slower: Finance (not applicable), Tech (8-15 days), Health (8-15 days)

**Importance:** Creators with <7 day lag are trend leaders; score higher on Niche Fit and Momentum

**When to track:** Weekly for trend-dependent niches (Music, Dance, Comedy, Lifestyle)

**Where to find:**
- Research: SOUND_TRENDS_AND_CROSSOVER_RESEARCH.md (Sound Trend Mechanics section)
- Implementation: NICHE_TAXONOMY_IMPLEMENTATION.md (Sound Trend Integration section)
- Quick reference: NICHE_TAXONOMY_QUICK_REFERENCE.md (Sound Trend Decision Matrix)

---

### Crossover Synergy Matrix

**High-Success Pairs (Encourage):**
- Music + Dance (80% audience overlap, algorithm boost, CPM $0.035-0.10)
- Finance + Education (85% overlap, no penalty, CPM $0.08-0.18)

**Moderate-Success Pairs (Encourage with Structure):**
- Fitness + Music (60-70% overlap, -5% penalty, CPM $0.045-0.11)
- Music + Comedy (70% overlap, -3% penalty, CPM $0.03-0.085)
- Dance + Lifestyle (75% overlap, -8% penalty, CPM $0.035-0.09)

**Risky Pairs (Monitor & Mitigate):**
- Comedy + Finance (trust-killing, -30% penalty, CPM plummets)
- Lifestyle + Tech (broad, -20% penalty, avoid random mixing)
- Gaming + Fitness (different demographics, -12% penalty)

**Avoid Pairs:**
- Comedy + Finance (incompatible)
- Multiple unrelated niches (3+) without theme

**Where to find:**
- Strategy: NICHE_TAXONOMY_AND_ANALYTICS.md (Tier 2 section, crossover pairs table)
- Research: SOUND_TRENDS_AND_CROSSOVER_RESEARCH.md (Crossover Pattern Analysis section, detailed case studies)
- Quick reference: NICHE_TAXONOMY_QUICK_REFERENCE.md (Crossover decision matrix, Decision Tree 2)

---

### CPM Benchmarks by Niche (2026)

| Tier | Niche | CPM Range | Notes |
|---|---|---|---|
| Premium | Finance | $0.10–$0.20 | Highest margin |
| Premium | Tech/Web3 | $0.08–$0.22 | Similar to finance |
| Premium | Health/Medical | $0.08–$0.15 | Regulated, compliance needed |
| Premium | Education | $0.06–$0.15 | Variable; evergreen benefit |
| Mid | Fitness | $0.05–$0.10 | Strong sponsor interest |
| Mid | Music | $0.04–$0.12 | Sound-dependent |
| Mid | Dance | $0.03–$0.08 | Trend-dependent |
| Mid | Lifestyle | $0.03–$0.08 | Broad category |
| Mid | Gaming | $0.03–$0.06 | Young audience, lower CPM |
| Entry | Comedy | $0.02–$0.05 | Highest engagement, lowest CPM |

**Where to find:**
- Strategy: NICHE_TAXONOMY_AND_ANALYTICS.md (Primary categories table, tier 1-3)
- Quick reference: NICHE_TAXONOMY_QUICK_REFERENCE.md (CPM Tier lookup table)

---

## Implementation Timeline

### Phase 1: Foundation (Weeks 1-3)
**Task:** Set up database schema and Niche Fit Score calculation

**Docs to reference:**
- NICHE_TAXONOMY_IMPLEMENTATION.md (Database Schema, Niche Fit Score)
- NICHE_TAXONOMY_AND_ANALYTICS.md (Formula validation)

**Owner:** Engineering

---

### Phase 2: Integration (Weeks 4-6)
**Task:** Integrate scoring into campaign matching; build dashboard

**Docs to reference:**
- NICHE_TAXONOMY_IMPLEMENTATION.md (API & Query Patterns)
- NICHE_TAXONOMY_QUICK_REFERENCE.md (Decision Tree 3 for matching logic)

**Owner:** Engineering + Campaign Operations

---

### Phase 3: Monitoring (Weeks 7-10)
**Task:** Set up sound trend tracking and quarterly niche health reports

**Docs to reference:**
- SOUND_TRENDS_AND_CROSSOVER_RESEARCH.md (Weekly monitoring protocol)
- NICHE_TAXONOMY_IMPLEMENTATION.md (Niche Health Monitoring queries)

**Owner:** Analytics + Campaign Strategy

---

## Key Takeaways for Rising Tides

1. **Finance niches are the margin play.** 3-5x CPM premium justifies higher overhead. One finance creator can offset 10 entertainment creators in revenue.

2. **Music + Dance is the velocity sweet spot.** Fastest trend responsiveness. Highest synergy (algorithm boost, not penalty). Best for music label campaigns.

3. **Crossover creators unlock value if paired correctly.** Music+Dance, Finance+Education, Fitness+Music work well. Avoid: Comedy+Finance (trust-killing).

4. **Momentum > Size.** A rising 50K follower creator outperforms a stalling 500K creator. Prioritize booking momentum-rising creators.

5. **Sound trends are time-sensitive.** Peak window: days 3-7 of emergence. Deploying within 48 hours of trend emergence captures maximum CPM uplift (+35-40%) and algorithmic boost.

6. **Consistency unlocks algorithm advantage.** Creators posting 4-5x/week reach new audiences 3-4x faster than 1x/week creators. Coach portfolio consistency.

7. **Niche health is predictable.** Monitor quarterly signals (sound adoption, CPM, creator growth) to forecast which niches to lean into or reduce exposure.

---

## Version History

| Version | Date | Changes |
|---|---|---|
| 1.0 | June 2026 | Initial four-document system: Strategy, Implementation, Research, Quick Reference |

---

## Document Ownership & Updates

| Document | Owner | Review Cycle | Next Review |
|---|---|---|---|
| NICHE_TAXONOMY_AND_ANALYTICS.md | Campaign Operations | Quarterly | September 2026 |
| NICHE_TAXONOMY_IMPLEMENTATION.md | Engineering | As-needed | September 2026 |
| SOUND_TRENDS_AND_CROSSOVER_RESEARCH.md | Campaign Strategy | Monthly | August 2026 |
| NICHE_TAXONOMY_QUICK_REFERENCE.md | Campaign Operations | Weekly (CPM tables) | Ongoing |

---

**System Status:** Production-ready  
**Last Updated:** June 2026  
**Next Full Review:** September 2026
