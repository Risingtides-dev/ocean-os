# Niche Taxonomy Implementation Guide

**Purpose:** Practical implementation playbook for the niche taxonomy strategy defined in `NICHE_TAXONOMY_AND_ANALYTICS.md`

**Date:** June 2026  
**Audience:** Engineering, Analytics, Campaign Operations

---

## Table of Contents

1. [Database Schema & Data Model](#database-schema--data-model)
2. [Scoring Engine Implementation](#scoring-engine-implementation)
3. [Crossover Pattern Detection](#crossover-pattern-detection)
4. [Sound Trend Integration](#sound-trend-integration)
5. [Niche Health Monitoring](#niche-health-monitoring)
6. [API & Query Patterns](#api--query-patterns)
7. [Testing & Validation](#testing--validation)

---

## Database Schema & Data Model

### Core Tables Extension

The `creators` table already has `niches` column (JSONB). Extend it with performance metrics:

```sql
-- New columns for creators table
ALTER TABLE creators ADD COLUMN IF NOT EXISTS (
    primary_niche VARCHAR(100),           -- Single niche for TikTok algorithm clarity
    secondary_niche VARCHAR(100),         -- Adjacent niche (optional)
    engagement_rate FLOAT DEFAULT 0.0,    -- Current ER (calculated, updated weekly)
    avg_cpm_earned FLOAT DEFAULT 0.0,     -- Tracked CPM (actual vs benchmark)
    follower_count BIGINT DEFAULT 0,      -- Cached follower count
    follower_growth_90d FLOAT DEFAULT 0.0,  -- % growth last 90 days
    last_video_date TIMESTAMP,            -- Most recent post
    avg_posts_per_week FLOAT DEFAULT 0.0, -- Content consistency metric
    sound_trend_adoption_lag_avg INT DEFAULT 0,  -- Avg days from trend emergence to use
    booking_velocity INT DEFAULT 0,       -- Campaigns booked in last 90 days
    niche_credibility_score INT DEFAULT 0,  -- 0-100: signal strength for niche authority
    is_crossover_candidate BOOLEAN DEFAULT FALSE,  -- Identified as multi-niche capable
    last_metrics_update TIMESTAMP         -- When engagement/CPM data was last refreshed
);

-- Creator performance tracking (time-series)
CREATE TABLE IF NOT EXISTS creator_performance_history (
    id SERIAL PRIMARY KEY,
    creator_id INT NOT NULL REFERENCES creators(id) ON DELETE CASCADE,
    measurement_date DATE NOT NULL,
    engagement_rate FLOAT,
    cpm_earned FLOAT,
    follower_count BIGINT,
    posts_this_week INT,
    sound_trend_lag INT,  -- Avg lag from this week
    booking_count INT,    -- Campaigns booked this week
    created_at TIMESTAMP DEFAULT NOW(),
    UNIQUE(creator_id, measurement_date)
);

-- Niche health dashboard (aggregated metrics)
CREATE TABLE IF NOT EXISTS niche_health_metrics (
    id SERIAL PRIMARY KEY,
    niche_slug VARCHAR(100) NOT NULL UNIQUE,
    measurement_date DATE NOT NULL,
    avg_engagement_rate FLOAT,
    median_engagement_rate FLOAT,
    avg_cpm FLOAT,
    creator_count INT,
    active_creator_count INT,  -- Posted in last 30 days
    avg_follower_growth_90d FLOAT,
    avg_sound_adoption_lag INT,
    campaigns_per_creator_90d FLOAT,
    consistency_pct FLOAT,  -- % posting 3+ times/week
    trend_adaptation_pct FLOAT,  -- % adopting within 14 days
    created_at TIMESTAMP DEFAULT NOW(),
    UNIQUE(niche_slug, measurement_date)
);

-- Sound trend tracking (for adoption lag calculation)
CREATE TABLE IF NOT EXISTS sound_trends (
    id SERIAL PRIMARY KEY,
    trend_name VARCHAR(255) NOT NULL,
    niche_category VARCHAR(100),  -- E.g., "music", "dance", "comedy"
    emergence_date TIMESTAMP NOT NULL,
    peak_date TIMESTAMP,
    decline_date TIMESTAMP,
    est_duration_days INT,
    peak_usage_count INT,  -- Videos using trend at peak
    status VARCHAR(50),  -- emerging, peak, saturated, declining
    created_at TIMESTAMP DEFAULT NOW(),
    UNIQUE(trend_name, emergence_date)
);

-- Creator sound trend adoption (for adoption lag tracking)
CREATE TABLE IF NOT EXISTS creator_sound_adoptions (
    id SERIAL PRIMARY KEY,
    creator_id INT NOT NULL REFERENCES creators(id) ON DELETE CASCADE,
    sound_trend_id INT NOT NULL REFERENCES sound_trends(id) ON DELETE CASCADE,
    first_use_date TIMESTAMP NOT NULL,
    days_from_emergence INT,  -- Calculated: emergence_date - first_use_date
    video_url TEXT,
    views_at_adoption INT,
    engagement_rate_at_adoption FLOAT,
    created_at TIMESTAMP DEFAULT NOW(),
    UNIQUE(creator_id, sound_trend_id)
);

-- Niche fit scores (pre-calculated for campaigns)
CREATE TABLE IF NOT EXISTS creator_campaign_scores (
    id SERIAL PRIMARY KEY,
    creator_id INT NOT NULL REFERENCES creators(id) ON DELETE CASCADE,
    campaign_id INT NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
    niche_fit_score FLOAT,  -- 0-100
    primary_match_score INT,
    engagement_strength_score INT,
    audience_quality_score INT,
    trend_alignment_score INT,
    cpm_efficiency_score INT,
    momentum_score FLOAT,  -- 0-100
    follower_growth_component FLOAT,
    booking_velocity_component FLOAT,
    engagement_trend_component FLOAT,
    sound_adoption_component FLOAT,
    consistency_component FLOAT,
    calculated_at TIMESTAMP DEFAULT NOW(),
    UNIQUE(creator_id, campaign_id)
);

-- Crossover performance tracking
CREATE TABLE IF NOT EXISTS creator_crossover_patterns (
    id SERIAL PRIMARY KEY,
    creator_id INT NOT NULL REFERENCES creators(id) ON DELETE CASCADE,
    primary_niche VARCHAR(100),
    secondary_niche VARCHAR(100),
    dual_niche_reach_pct FLOAT,  -- % of content spanning both niches
    primary_only_pct FLOAT,
    secondary_only_pct FLOAT,
    algorithm_penalty_observed FLOAT,  -- Measured reach reduction (%)
    audience_overlap_pct FLOAT,  -- % of followers interested in both
    recommendation_status VARCHAR(50),  -- active, potential, avoid
    last_analysis_date DATE,
    created_at TIMESTAMP DEFAULT NOW(),
    UNIQUE(creator_id, primary_niche, secondary_niche)
);
```

### Niche Enumeration

Store the taxonomy as a reference table for data integrity:

```sql
CREATE TABLE IF NOT EXISTS niche_categories (
    id SERIAL PRIMARY KEY,
    slug VARCHAR(100) UNIQUE NOT NULL,
    name VARCHAR(255) NOT NULL,
    tier INT NOT NULL,  -- 1 (primary), 2 (sub-niche), 3 (emerging)
    parent_slug VARCHAR(100),  -- For hierarchical relationships
    cpm_min FLOAT,
    cpm_max FLOAT,
    engagement_min FLOAT,
    engagement_max FLOAT,
    trend_velocity VARCHAR(50),  -- fast, moderate, slow
    description TEXT,
    example_creators TEXT[],  -- Array of example creator handles
    created_at TIMESTAMP DEFAULT NOW(),
    FOREIGN KEY (parent_slug) REFERENCES niche_categories(slug)
);

-- Seed data example:
INSERT INTO niche_categories (slug, name, tier, parent_slug, cpm_min, cpm_max, engagement_min, engagement_max, trend_velocity, description)
VALUES 
('music-production', 'Music Production/Beat Making', 2, 'music-sound', 0.04, 0.12, 0.02, 0.04, 'fast', 'Production tutorials, studio tips, beatpack drops'),
('music-performance', 'Music Performance', 2, 'music-sound', 0.04, 0.12, 0.02, 0.04, 'fast', 'Live singing, instrumental covers, music videos'),
('dance-trending', 'Trending Dance Challenges', 2, 'dance-choreography', 0.03, 0.08, 0.03, 0.05, 'very-fast', 'TikTok dance format, choreography breakdowns'),
('finance-personal', 'Personal Finance/Money Tips', 2, 'finance-business', 0.10, 0.20, 0.01, 0.03, 'slow', 'Budgeting, saving, investment basics'),
-- ... more entries
;
```

---

## Scoring Engine Implementation

### Niche Fit Score Calculation

Python implementation (integrate into Campaign Hub or Ocean runtime):

```python
from dataclasses import dataclass
from typing import Optional
from datetime import datetime, timedelta

@dataclass
class NicheFitScoreRequest:
    creator_id: int
    campaign_id: int
    creator_primary_niche: str
    creator_secondary_niche: Optional[str]
    campaign_target_niche: str
    creator_engagement_rate: float
    niche_avg_engagement_rate: float
    creator_follower_count: int
    creator_authenticity_pct: float  # 0-100
    audience_demographic_match: float  # 0-100 (brand research)
    audience_purchase_intent: str  # "high", "medium", "low"
    creator_cpm_earned: float
    niche_cpm_benchmark: float
    sound_trend_adoption_lag_days: Optional[int]  # None if not applicable
    campaign_is_sound_dependent: bool

@dataclass
class NicheFitScore:
    total_score: float
    primary_match: float
    engagement_strength: float
    audience_quality: float
    trend_alignment: float
    cpm_efficiency: float
    recommendation: str  # "tier-1", "tier-2", "tier-3", "avoid"

def calculate_niche_fit_score(req: NicheFitScoreRequest) -> NicheFitScore:
    """
    Calculate Niche Fit Score per formula in NICHE_TAXONOMY_AND_ANALYTICS.md
    Niche Fit = (Primary × 0.50) + (Engagement × 0.20) + (Audience × 0.15) + (Trend × 0.10) + (CPM × 0.05)
    """
    
    # Component 1: Primary Match (0-100)
    primary_match = calculate_primary_match(
        req.creator_primary_niche,
        req.creator_secondary_niche,
        req.campaign_target_niche
    )
    
    # Component 2: Engagement Strength (0-100)
    engagement_strength = calculate_engagement_strength(
        req.creator_engagement_rate,
        req.niche_avg_engagement_rate
    )
    
    # Component 3: Audience Quality (0-100)
    audience_quality = calculate_audience_quality(
        req.audience_demographic_match,
        req.creator_authenticity_pct,
        req.audience_purchase_intent
    )
    
    # Component 4: Trend Alignment (0-100)
    trend_alignment = calculate_trend_alignment(
        req.sound_trend_adoption_lag_days,
        req.campaign_is_sound_dependent
    )
    
    # Component 5: CPM Efficiency (0-100)
    cpm_efficiency = calculate_cpm_efficiency(
        req.creator_cpm_earned,
        req.niche_cpm_benchmark
    )
    
    total = (
        primary_match * 0.50 +
        engagement_strength * 0.20 +
        audience_quality * 0.15 +
        trend_alignment * 0.10 +
        cpm_efficiency * 0.05
    )
    
    recommendation = categorize_fit(total)
    
    return NicheFitScore(
        total_score=round(total, 2),
        primary_match=round(primary_match, 2),
        engagement_strength=round(engagement_strength, 2),
        audience_quality=round(audience_quality, 2),
        trend_alignment=round(trend_alignment, 2),
        cpm_efficiency=round(cpm_efficiency, 2),
        recommendation=recommendation
    )

def calculate_primary_match(primary: str, secondary: Optional[str], campaign_niche: str) -> float:
    """
    Exact match = 100
    Primary + strong secondary match = 80-99
    Adjacent niche = 60-79
    Audience overlap = 40-59
    Minimal/none = 0-39
    """
    if primary == campaign_niche:
        return 100.0
    
    if secondary and secondary == campaign_niche:
        return 85.0
    
    # Check adjacency map (defined in niche_categories.parent_slug)
    adjacent_pairs = {
        ('music-sound', 'dance-choreography'): 80,
        ('fitness', 'music-sound'): 75,
        ('dance-choreography', 'lifestyle'): 78,
        ('finance-business', 'education'): 82,
        ('tech-web3', 'education'): 80,
        ('fitness', 'lifestyle'): 75,
        ('comedy', 'music-sound'): 70,
        ('gaming', 'comedy'): 65,
    }
    
    pair = (primary, campaign_niche)
    if pair in adjacent_pairs:
        return float(adjacent_pairs[pair])
    
    pair = (campaign_niche, primary)
    if pair in adjacent_pairs:
        return float(adjacent_pairs[pair])
    
    return 45.0  # Default for partial overlap

def calculate_engagement_strength(creator_er: float, niche_avg_er: float) -> float:
    """
    Score = (creator_er ÷ niche_avg_er) × 100, capped at 100
    """
    if niche_avg_er == 0:
        return 50.0
    
    ratio = creator_er / niche_avg_er
    score = ratio * 100
    return min(score, 100.0)

def calculate_audience_quality(demo_match: float, authenticity: float, intent: str) -> float:
    """
    Weighted: demographics (40) + authenticity (30) + intent (30)
    """
    demo_score = (demo_match / 100) * 40
    
    auth_score = (authenticity / 100) * 30
    
    intent_scores = {"high": 30, "medium": 20, "low": 10}
    intent_score = intent_scores.get(intent, 10)
    
    return demo_score + auth_score + intent_score

def calculate_trend_alignment(adoption_lag: Optional[int], is_sound_dependent: bool) -> float:
    """
    For sound-dependent campaigns:
    - <7 days: 100 (leading)
    - 7-14 days: 80 (timely)
    - 14-30 days: 60 (catching up)
    - >30 days: 40 (lagging)
    
    For non-sound campaigns: 100 (not penalized)
    """
    if not is_sound_dependent or adoption_lag is None:
        return 100.0
    
    if adoption_lag < 7:
        return 100.0
    elif adoption_lag < 14:
        return 80.0
    elif adoption_lag < 30:
        return 60.0
    else:
        return 40.0

def calculate_cpm_efficiency(creator_cpm: float, benchmark_cpm: float) -> float:
    """
    Score = (creator_cpm ÷ benchmark_cpm) × 100, capped at 100
    """
    if benchmark_cpm == 0:
        return 50.0
    
    ratio = creator_cpm / benchmark_cpm
    score = ratio * 100
    return min(score, 100.0)

def categorize_fit(score: float) -> str:
    if score >= 85:
        return "tier-1"
    elif score >= 70:
        return "tier-2"
    elif score >= 50:
        return "tier-3"
    else:
        return "avoid"
```

### Creator Momentum Score Calculation

```python
@dataclass
class MomentumScoreRequest:
    creator_id: int
    follower_growth_90d_pct: float  # 0-100
    campaigns_booked_90d: int
    engagement_rate_90d_current: float
    engagement_rate_90d_30d_ago: float
    sound_adoption_lag_avg: Optional[int]
    is_trend_dependent_niche: bool
    posts_per_week_90d_avg: float

@dataclass
class MomentumScore:
    total_score: float
    follower_growth_component: float
    booking_velocity_component: float
    engagement_trend_component: float
    sound_adoption_component: float
    consistency_component: float
    interpretation: str

def calculate_creator_momentum_score(req: MomentumScoreRequest) -> MomentumScore:
    """
    Momentum = (Follower Growth × 0.25) + (Booking Velocity × 0.25) + 
               (Engagement Trend × 0.25) + (Sound Adoption × 0.15) + 
               (Consistency × 0.10)
    """
    
    # Component 1: Follower Growth (0-100)
    follower_growth = score_follower_growth(req.follower_growth_90d_pct)
    
    # Component 2: Booking Velocity (0-100)
    booking_velocity = score_booking_velocity(req.campaigns_booked_90d)
    
    # Component 3: Engagement Trend (0-100)
    engagement_trend = score_engagement_trend(
        req.engagement_rate_90d_current,
        req.engagement_rate_90d_30d_ago
    )
    
    # Component 4: Sound Adoption (0-100)
    sound_adoption = score_sound_adoption(
        req.sound_adoption_lag_avg,
        req.is_trend_dependent_niche
    )
    
    # Component 5: Consistency (0-100)
    consistency = score_consistency(req.posts_per_week_90d_avg)
    
    total = (
        follower_growth * 0.25 +
        booking_velocity * 0.25 +
        engagement_trend * 0.25 +
        sound_adoption * 0.15 +
        consistency * 0.10
    )
    
    return MomentumScore(
        total_score=round(total, 2),
        follower_growth_component=round(follower_growth, 2),
        booking_velocity_component=round(booking_velocity, 2),
        engagement_trend_component=round(engagement_trend, 2),
        sound_adoption_component=round(sound_adoption, 2),
        consistency_component=round(consistency, 2),
        interpretation=interpret_momentum(total)
    )

def score_follower_growth(growth_pct: float) -> float:
    if growth_pct > 20:
        return 100.0
    elif growth_pct > 10:
        return 80.0
    elif growth_pct > 5:
        return 60.0
    elif growth_pct > 0:
        return 40.0
    else:
        return 10.0

def score_booking_velocity(campaigns: int) -> float:
    if campaigns >= 5:
        return 100.0
    elif campaigns >= 3:
        return 80.0
    elif campaigns >= 1:
        return 60.0
    elif campaigns == 0:
        return 20.0
    else:
        return 50.0  # Default for new creators

def score_engagement_trend(current_er: float, previous_er: float) -> float:
    if previous_er == 0:
        return 60.0
    
    change = ((current_er - previous_er) / previous_er) * 100
    
    if change > 5:
        return 100.0
    elif change > 2:
        return 80.0
    elif change > -2:
        return 60.0
    elif change > -5:
        return 40.0
    else:
        return 10.0

def score_sound_adoption(lag_avg: Optional[int], is_trend_dependent: bool) -> float:
    if not is_trend_dependent or lag_avg is None:
        return 75.0
    
    if lag_avg < 7:
        return 100.0
    elif lag_avg < 14:
        return 80.0
    elif lag_avg < 30:
        return 60.0
    else:
        return 30.0

def score_consistency(posts_per_week: float) -> float:
    if posts_per_week >= 5:
        return 100.0
    elif posts_per_week >= 3:
        return 80.0
    elif posts_per_week >= 1:
        return 60.0
    else:
        return 30.0

def interpret_momentum(score: float) -> str:
    if score >= 80:
        return "High momentum - excellent campaign candidate"
    elif score >= 60:
        return "Moderate momentum - solid performer"
    elif score >= 40:
        return "Low momentum - needs attention or emerging"
    else:
        return "Declining momentum - at risk"
```

---

## Crossover Pattern Detection

### Identifying High-Success Crossovers

```python
@dataclass
class CrossoverAnalysis:
    creator_id: int
    primary_niche: str
    secondary_niche: str
    content_distribution_pct: dict  # {primary_only, dual, secondary_only}
    algorithm_penalty_observed: float  # % reach reduction
    audience_overlap_pct: float
    recommendation: str  # "encourage", "monitor", "avoid"
    reasoning: str

HIGH_SUCCESS_CROSSOVERS = {
    ('music-sound', 'dance-choreography'): {
        'expected_algorithm_penalty': 0,  # Naturally synergistic
        'audience_overlap': 85,
        'recommendation': 'encourage',
        'cpm_blend_low': 0.035,
        'cpm_blend_high': 0.10,
    },
    ('fitness', 'music-sound'): {
        'expected_algorithm_penalty': 5,
        'audience_overlap': 70,
        'recommendation': 'encourage',
        'cpm_blend_low': 0.045,
        'cpm_blend_high': 0.11,
    },
    ('dance-choreography', 'lifestyle'): {
        'expected_algorithm_penalty': 8,
        'audience_overlap': 75,
        'recommendation': 'encourage',
        'cpm_blend_low': 0.035,
        'cpm_blend_high': 0.09,
    },
    ('finance-business', 'education'): {
        'expected_algorithm_penalty': 3,
        'audience_overlap': 80,
        'recommendation': 'encourage',
        'cpm_blend_low': 0.08,
        'cpm_blend_high': 0.18,
    },
}

RISKY_CROSSOVERS = {
    ('comedy', 'finance-business'): {
        'expected_algorithm_penalty': 30,
        'brand_safety_risk': 'high',
        'recommendation': 'avoid',
        'reasoning': 'Finance demands trust; comedy undermines credibility',
    },
    ('gaming', 'finance-business'): {
        'expected_algorithm_penalty': 25,
        'audience_mismatch': True,
        'recommendation': 'avoid',
    },
    ('lifestyle', 'tech-web3'): {
        'expected_algorithm_penalty': 20,
        'niche_confusion': True,
        'recommendation': 'high-risk',
        'mitigation': 'Separate content days; clear thematic distinction',
    },
}

def detect_crossover_potential(creator_id: int, content_history: list[dict]) -> CrossoverAnalysis:
    """
    Analyze creator's recent content to identify if they're naturally multi-niche.
    Content history: [{"niche": "music-sound", "engagement": 0.04, "views": 50000}, ...]
    """
    
    niche_distribution = {}
    for item in content_history:
        niche = item.get('niche', 'unknown')
        niche_distribution[niche] = niche_distribution.get(niche, 0) + 1
    
    if len(niche_distribution) < 2:
        return None  # Not a crossover candidate
    
    total_videos = sum(niche_distribution.values())
    
    # Identify primary and secondary
    sorted_niches = sorted(niche_distribution.items(), key=lambda x: x[1], reverse=True)
    primary_niche = sorted_niches[0][0]
    secondary_niche = sorted_niches[1][0] if len(sorted_niches) > 1 else None
    
    if not secondary_niche:
        return None
    
    primary_pct = (niche_distribution[primary_niche] / total_videos) * 100
    secondary_pct = (niche_distribution[secondary_niche] / total_videos) * 100
    other_pct = 100 - primary_pct - secondary_pct
    
    # Look up expected algorithm penalty
    pair_key = (primary_niche, secondary_niche)
    reverse_key = (secondary_niche, primary_niche)
    
    if pair_key in HIGH_SUCCESS_CROSSOVERS:
        expected_penalty = HIGH_SUCCESS_CROSSOVERS[pair_key]['expected_algorithm_penalty']
        recommendation = 'encourage'
    elif reverse_key in HIGH_SUCCESS_CROSSOVERS:
        expected_penalty = HIGH_SUCCESS_CROSSOVERS[reverse_key]['expected_algorithm_penalty']
        recommendation = 'encourage'
    elif pair_key in RISKY_CROSSOVERS:
        expected_penalty = RISKY_CROSSOVERS[pair_key]['expected_algorithm_penalty']
        recommendation = RISKY_CROSSOVERS[pair_key]['recommendation']
    else:
        expected_penalty = 15  # Default moderate penalty
        recommendation = 'monitor'
    
    # Estimate actual penalty by comparing performance
    primary_only_videos = [v for v in content_history if v.get('niche') == primary_niche]
    dual_videos = [v for v in content_history if v.get('niche') in [primary_niche, secondary_niche]]
    
    if primary_only_videos and dual_videos:
        avg_er_primary_only = sum(v.get('engagement_rate', 0) for v in primary_only_videos) / len(primary_only_videos)
        avg_er_dual = sum(v.get('engagement_rate', 0) for v in dual_videos) / len(dual_videos)
        
        if avg_er_primary_only > 0:
            observed_penalty = ((avg_er_primary_only - avg_er_dual) / avg_er_primary_only) * 100
        else:
            observed_penalty = expected_penalty
    else:
        observed_penalty = expected_penalty
    
    return CrossoverAnalysis(
        creator_id=creator_id,
        primary_niche=primary_niche,
        secondary_niche=secondary_niche,
        content_distribution_pct={
            'primary_only': round(primary_pct, 1),
            'dual': round((1 - primary_pct/100 - secondary_pct/100) * 100, 1),
            'secondary_only': round(secondary_pct, 1),
        },
        algorithm_penalty_observed=round(observed_penalty, 1),
        audience_overlap_pct=estimate_audience_overlap(primary_niche, secondary_niche),
        recommendation=recommendation,
        reasoning=generate_crossover_reasoning(primary_niche, secondary_niche, recommendation)
    )
```

---

## Sound Trend Integration

### Adoption Lag Tracking

```python
from datetime import datetime, timedelta

def track_sound_trend_adoption(
    creator_id: int,
    sound_trend_id: int,
    trend_emergence_date: datetime,
    creator_first_use_date: datetime,
    video_url: str,
    metrics: dict
) -> int:
    """
    Record when creator adopted a sound trend.
    Returns: days_from_emergence
    """
    
    days_lag = (creator_first_use_date - trend_emergence_date).days
    
    # Store in database
    creator_sound_adoption = CreatorSoundAdoption(
        creator_id=creator_id,
        sound_trend_id=sound_trend_id,
        first_use_date=creator_first_use_date,
        days_from_emergence=days_lag,
        video_url=video_url,
        views_at_adoption=metrics.get('views', 0),
        engagement_rate_at_adoption=metrics.get('engagement_rate', 0.0),
    )
    db.session.add(creator_sound_adoption)
    db.session.commit()
    
    return days_lag

def calculate_avg_adoption_lag(creator_id: int, lookback_days: int = 90) -> int:
    """
    Calculate average sound trend adoption lag for creator over past N days.
    """
    cutoff_date = datetime.now() - timedelta(days=lookback_days)
    
    adoptions = db.session.query(CreatorSoundAdoption).join(
        SoundTrend
    ).filter(
        CreatorSoundAdoption.creator_id == creator_id,
        SoundTrend.emergence_date >= cutoff_date
    ).all()
    
    if not adoptions:
        return None
    
    avg_lag = sum(a.days_from_emergence for a in adoptions) / len(adoptions)
    return round(avg_lag)

def identify_trend_leaders(niche: str, trend_id: int, lookback_hours: int = 72) -> list[int]:
    """
    Identify creators in a niche who adopted a trend within 72 hours (trend leaders).
    These creators should be prioritized for future sound-based campaigns.
    """
    cutoff_date = datetime.now() - timedelta(hours=lookback_hours)
    
    leaders = db.session.query(CreatorSoundAdoption).join(
        SoundTrend
    ).join(
        Creator
    ).filter(
        SoundTrend.id == trend_id,
        CreatorSoundAdoption.first_use_date >= cutoff_date,
        Creator.primary_niche == niche
    ).order_by(CreatorSoundAdoption.first_use_date).all()
    
    return [adoption.creator_id for adoption in leaders]
```

### Sound Trend Monitoring

```python
def monitor_sound_trend_emergence(
    trend_name: str,
    niche_category: str,
    emergence_date: datetime
) -> dict:
    """
    Monitor a new sound trend and track metrics over its lifecycle.
    """
    
    trend = SoundTrend(
        trend_name=trend_name,
        niche_category=niche_category,
        emergence_date=emergence_date,
        status='emerging',
    )
    db.session.add(trend)
    db.session.commit()
    
    return {
        'trend_id': trend.id,
        'emergence_date': emergence_date,
        'monitoring_started': datetime.now(),
    }

def update_sound_trend_status(trend_id: int, metrics: dict) -> str:
    """
    Update sound trend status based on usage metrics.
    Statuses: emerging → peak → saturated → declining
    """
    
    trend = db.session.query(SoundTrend).filter_by(id=trend_id).first()
    if not trend:
        return None
    
    usage_count = metrics.get('usage_count', 0)
    days_since_emergence = (datetime.now() - trend.emergence_date).days
    
    if usage_count > 1000000 and days_since_emergence < 7:
        new_status = 'peak'
        trend.peak_date = datetime.now()
    elif usage_count > 500000 and days_since_emergence < 14:
        new_status = 'saturated'
    elif days_since_emergence > 30 or usage_count < 10000:
        new_status = 'declining'
        trend.decline_date = datetime.now()
    else:
        new_status = 'emerging'
    
    trend.status = new_status
    trend.peak_usage_count = max(trend.peak_usage_count or 0, usage_count)
    
    if trend.decline_date and trend.emergence_date:
        trend.est_duration_days = (trend.decline_date - trend.emergence_date).days
    
    db.session.commit()
    
    return new_status
```

---

## Niche Health Monitoring

### Quarterly Niche Health Dashboard

```python
from enum import Enum

class NicheHealthStatus(str, Enum):
    RISING = "rising"
    STABLE = "stable"
    DECLINING = "declining"

@dataclass
class NicheHealthMetrics:
    niche_slug: str
    measurement_date: date
    avg_engagement_rate: float
    median_engagement_rate: float
    avg_cpm: float
    creator_count: int
    active_creator_count: int
    avg_follower_growth_90d: float
    avg_sound_adoption_lag: Optional[int]
    campaigns_per_creator_90d: float
    consistency_pct: float  # % posting 3+ times/week
    trend_adaptation_pct: float  # % adopting within 14 days
    health_status: NicheHealthStatus
    trend_signals: dict[str, bool]

def calculate_niche_health_quarterly(niche_slug: str) -> NicheHealthMetrics:
    """
    Calculate comprehensive niche health score.
    Stores result in niche_health_metrics table for history tracking.
    """
    
    today = date.today()
    
    # Get all active creators in niche
    creators = db.session.query(Creator).filter(
        Creator.primary_niche == niche_slug,
        Creator.status == 'active'
    ).all()
    
    if not creators:
        return None
    
    creator_ids = [c.id for c in creators]
    
    # Performance metrics
    engagement_rates = [c.engagement_rate for c in creators if c.engagement_rate]
    cpms = [c.avg_cpm_earned for c in creators if c.avg_cpm_earned]
    follower_growth_rates = [c.follower_growth_90d for c in creators if c.follower_growth_90d]
    adoption_lags = [c.sound_trend_adoption_lag_avg for c in creators if c.sound_trend_adoption_lag_avg]
    
    avg_engagement = sum(engagement_rates) / len(engagement_rates) if engagement_rates else 0.0
    median_engagement = sorted(engagement_rates)[len(engagement_rates)//2] if engagement_rates else 0.0
    avg_cpm = sum(cpms) / len(cpms) if cpms else 0.0
    
    # Activity metrics
    active_creators = db.session.query(Creator).filter(
        Creator.id.in_(creator_ids),
        Creator.last_video_date >= (today - timedelta(days=30))
    ).count()
    
    consistency_pct = (db.session.query(Creator).filter(
        Creator.id.in_(creator_ids),
        Creator.avg_posts_per_week >= 3
    ).count() / len(creators)) * 100
    
    # Sound trend adaptation
    adoption_lags = [l for l in adoption_lags if l]
    avg_adoption_lag = sum(adoption_lags) / len(adoption_lags) if adoption_lags else 30
    trend_adaptation_pct = (sum(1 for l in adoption_lags if l <= 14) / len(adoption_lags)) * 100 if adoption_lags else 0
    
    # Booking metrics
    total_campaigns_90d = db.session.query(func.count(Creator.id)).join(
        CreatorCampaignScores
    ).filter(
        Creator.id.in_(creator_ids),
        CreatorCampaignScores.calculated_at >= (today - timedelta(days=90))
    ).scalar() or 0
    
    campaigns_per_creator = total_campaigns_90d / len(creators) if creators else 0
    
    # Determine health status
    trend_signals = {
        'sound_adoption_compressed': avg_adoption_lag < 10,
        'creator_growth': follower_growth_rates and sum(follower_growth_rates)/len(follower_growth_rates) > 15,
        'cpm_increasing': True,  # Would compare to previous quarter
        'high_consistency': consistency_pct > 70,
        'strong_booking': campaigns_per_creator > 1.5,
        'engagement_above_baseline': avg_engagement > 0.04,
        'high_trend_adoption': trend_adaptation_pct > 60,
    }
    
    rising_signals = sum(1 for v in trend_signals.values() if v)
    health_status = NicheHealthStatus.RISING if rising_signals >= 5 else \
                   NicheHealthStatus.DECLINING if rising_signals <= 2 else \
                   NicheHealthStatus.STABLE
    
    metrics = NicheHealthMetrics(
        niche_slug=niche_slug,
        measurement_date=today,
        avg_engagement_rate=round(avg_engagement, 4),
        median_engagement_rate=round(median_engagement, 4),
        avg_cpm=round(avg_cpm, 2),
        creator_count=len(creators),
        active_creator_count=active_creators,
        avg_follower_growth_90d=round(sum(follower_growth_rates)/len(follower_growth_rates), 1) if follower_growth_rates else 0,
        avg_sound_adoption_lag=int(avg_adoption_lag),
        campaigns_per_creator_90d=round(campaigns_per_creator, 2),
        consistency_pct=round(consistency_pct, 1),
        trend_adaptation_pct=round(trend_adaptation_pct, 1),
        health_status=health_status,
        trend_signals=trend_signals,
    )
    
    # Store in database
    db_metrics = NicheHealthMetric(
        niche_slug=niche_slug,
        measurement_date=today,
        avg_engagement_rate=metrics.avg_engagement_rate,
        median_engagement_rate=metrics.median_engagement_rate,
        avg_cpm=metrics.avg_cpm,
        creator_count=metrics.creator_count,
        active_creator_count=metrics.active_creator_count,
        avg_follower_growth_90d=metrics.avg_follower_growth_90d,
        avg_sound_adoption_lag=metrics.avg_sound_adoption_lag,
        campaigns_per_creator_90d=metrics.campaigns_per_creator_90d,
        consistency_pct=metrics.consistency_pct,
        trend_adaptation_pct=metrics.trend_adaptation_pct,
    )
    db.session.add(db_metrics)
    db.session.commit()
    
    return metrics

def compare_niche_health_quarterly(niche_slug: str) -> dict:
    """
    Compare current quarter to previous quarter.
    Identify rising vs declining trends.
    """
    
    today = date.today()
    current_quarter_start = date(today.year, ((today.month - 1) // 3) * 3 + 1, 1)
    previous_quarter_start = current_quarter_start - timedelta(days=90)
    
    current_metrics = db.session.query(NicheHealthMetric).filter(
        NicheHealthMetric.niche_slug == niche_slug,
        NicheHealthMetric.measurement_date >= current_quarter_start
    ).order_by(NicheHealthMetric.measurement_date.desc()).first()
    
    previous_metrics = db.session.query(NicheHealthMetric).filter(
        NicheHealthMetric.niche_slug == niche_slug,
        NicheHealthMetric.measurement_date >= previous_quarter_start,
        NicheHealthMetric.measurement_date < current_quarter_start
    ).order_by(NicheHealthMetric.measurement_date.desc()).first()
    
    if not current_metrics or not previous_metrics:
        return None
    
    return {
        'niche_slug': niche_slug,
        'current_quarter': {
            'avg_engagement': current_metrics.avg_engagement_rate,
            'avg_cpm': current_metrics.avg_cpm,
            'creator_count': current_metrics.creator_count,
            'consistency_pct': current_metrics.consistency_pct,
        },
        'previous_quarter': {
            'avg_engagement': previous_metrics.avg_engagement_rate,
            'avg_cpm': previous_metrics.avg_cpm,
            'creator_count': previous_metrics.creator_count,
            'consistency_pct': previous_metrics.consistency_pct,
        },
        'changes': {
            'engagement_change_pct': ((current_metrics.avg_engagement_rate - previous_metrics.avg_engagement_rate) / previous_metrics.avg_engagement_rate * 100) if previous_metrics.avg_engagement_rate > 0 else 0,
            'cpm_change_pct': ((current_metrics.avg_cpm - previous_metrics.avg_cpm) / previous_metrics.avg_cpm * 100) if previous_metrics.avg_cpm > 0 else 0,
            'creator_growth': current_metrics.creator_count - previous_metrics.creator_count,
            'consistency_improvement': current_metrics.consistency_pct - previous_metrics.consistency_pct,
        },
        'verdict': 'RISING' if sum([
            current_metrics.avg_engagement_rate > previous_metrics.avg_engagement_rate,
            current_metrics.avg_cpm > previous_metrics.avg_cpm,
            current_metrics.creator_count > previous_metrics.creator_count,
            current_metrics.consistency_pct > previous_metrics.consistency_pct,
        ]) >= 3 else 'STABLE' if sum([
            current_metrics.avg_engagement_rate > previous_metrics.avg_engagement_rate * 0.95,
            current_metrics.avg_cpm > previous_metrics.avg_cpm * 0.95,
        ]) >= 1 else 'DECLINING',
    }
```

---

## API & Query Patterns

### Campaign Matching Query

```sql
-- Find best creators for a campaign based on niche fit
SELECT 
    c.id,
    c.username,
    c.primary_niche,
    c.secondary_niche,
    c.follower_count,
    c.engagement_rate,
    c.avg_cpm_earned,
    ccs.niche_fit_score,
    ccs.momentum_score,
    ccs.primary_match_score,
    ccs.engagement_strength_score,
    CASE 
        WHEN ccs.niche_fit_score >= 85 THEN 'tier-1'
        WHEN ccs.niche_fit_score >= 70 THEN 'tier-2'
        WHEN ccs.niche_fit_score >= 50 THEN 'tier-3'
        ELSE 'avoid'
    END as recommendation,
    ROW_NUMBER() OVER (ORDER BY ccs.niche_fit_score DESC) as rank
FROM creators c
LEFT JOIN creator_campaign_scores ccs ON c.id = ccs.creator_id AND ccs.campaign_id = $1
WHERE c.status = 'active'
    AND (c.primary_niche = $2 OR c.secondary_niche = $2)
    AND c.engagement_rate > 0.01
ORDER BY ccs.niche_fit_score DESC NULLS LAST
LIMIT 50;
```

### Crossover Analysis Query

```sql
-- Identify creators with strong dual-niche performance
SELECT 
    c.id,
    c.username,
    c.primary_niche,
    c.secondary_niche,
    ccp.dual_niche_reach_pct,
    ccp.primary_only_pct,
    ccp.secondary_only_pct,
    ccp.algorithm_penalty_observed,
    ccp.audience_overlap_pct,
    ccp.recommendation_status,
    c.engagement_rate,
    c.avg_cpm_earned
FROM creators c
JOIN creator_crossover_patterns ccp ON c.id = ccp.creator_id
WHERE ccp.recommendation_status = 'active'
    AND c.engagement_rate > 0.02
ORDER BY ccp.algorithm_penalty_observed ASC,
         ccp.audience_overlap_pct DESC;
```

### Niche Health Trending

```sql
-- Quarterly niche health comparison (rising vs declining)
SELECT 
    nhm_current.niche_slug,
    nhm_current.avg_engagement_rate as current_er,
    nhm_prev.avg_engagement_rate as previous_er,
    ROUND(((nhm_current.avg_engagement_rate - nhm_prev.avg_engagement_rate) / nhm_prev.avg_engagement_rate * 100)::numeric, 1) as er_change_pct,
    nhm_current.avg_cpm as current_cpm,
    nhm_prev.avg_cpm as previous_cpm,
    ROUND(((nhm_current.avg_cpm - nhm_prev.avg_cpm) / nhm_prev.avg_cpm * 100)::numeric, 1) as cpm_change_pct,
    nhm_current.creator_count,
    nhm_current.consistency_pct,
    CASE 
        WHEN ((nhm_current.avg_engagement_rate - nhm_prev.avg_engagement_rate) / nhm_prev.avg_engagement_rate > 0.05)
            AND ((nhm_current.avg_cpm - nhm_prev.avg_cpm) / nhm_prev.avg_cpm > 0.05)
            THEN 'RISING'
        WHEN ((nhm_current.avg_engagement_rate - nhm_prev.avg_engagement_rate) / nhm_prev.avg_engagement_rate < -0.10)
            OR ((nhm_current.avg_cpm - nhm_prev.avg_cpm) / nhm_prev.avg_cpm < -0.15)
            THEN 'DECLINING'
        ELSE 'STABLE'
    END as health_status
FROM niche_health_metrics nhm_current
JOIN niche_health_metrics nhm_prev ON nhm_current.niche_slug = nhm_prev.niche_slug
WHERE nhm_current.measurement_date = CURRENT_DATE
    AND nhm_prev.measurement_date = CURRENT_DATE - INTERVAL '90 days'
ORDER BY cpm_change_pct DESC;
```

---

## Testing & Validation

### Unit Tests for Scoring

```python
import pytest

class TestNicheFitScore:
    def test_exact_niche_match_scores_high(self):
        req = NicheFitScoreRequest(
            creator_id=1,
            campaign_id=1,
            creator_primary_niche='music-production',
            creator_secondary_niche=None,
            campaign_target_niche='music-production',
            creator_engagement_rate=0.035,
            niche_avg_engagement_rate=0.035,
            creator_follower_count=100000,
            creator_authenticity_pct=95,
            audience_demographic_match=100,
            audience_purchase_intent='high',
            creator_cpm_earned=0.10,
            niche_cpm_benchmark=0.08,
            sound_trend_adoption_lag_days=5,
            campaign_is_sound_dependent=True
        )
        result = calculate_niche_fit_score(req)
        assert result.primary_match == 100.0
        assert result.total_score >= 90
    
    def test_adjacent_niche_match_scores_moderately(self):
        req = NicheFitScoreRequest(
            creator_id=1,
            campaign_id=1,
            creator_primary_niche='dance-choreography',
            creator_secondary_niche=None,
            campaign_target_niche='music-production',
            creator_engagement_rate=0.04,
            niche_avg_engagement_rate=0.035,
            creator_follower_count=50000,
            creator_authenticity_pct=90,
            audience_demographic_match=85,
            audience_purchase_intent='medium',
            creator_cpm_earned=0.085,
            niche_cpm_benchmark=0.08,
            sound_trend_adoption_lag_days=6,
            campaign_is_sound_dependent=True
        )
        result = calculate_niche_fit_score(req)
        assert result.primary_match >= 75
        assert result.total_score >= 70
        assert result.recommendation == 'tier-1'
    
    def test_misaligned_niche_avoids(self):
        req = NicheFitScoreRequest(
            creator_id=1,
            campaign_id=1,
            creator_primary_niche='gaming',
            creator_secondary_niche=None,
            campaign_target_niche='finance-business',
            creator_engagement_rate=0.025,
            niche_avg_engagement_rate=0.025,
            creator_follower_count=200000,
            creator_authenticity_pct=85,
            audience_demographic_match=10,
            audience_purchase_intent='low',
            creator_cpm_earned=0.035,
            niche_cpm_benchmark=0.15,
            sound_trend_adoption_lag_days=None,
            campaign_is_sound_dependent=False
        )
        result = calculate_niche_fit_score(req)
        assert result.primary_match < 50
        assert result.recommendation == 'avoid'

class TestMomentumScore:
    def test_high_growth_creator_scores_high(self):
        req = MomentumScoreRequest(
            creator_id=1,
            follower_growth_90d_pct=25,
            campaigns_booked_90d=5,
            engagement_rate_90d_current=0.045,
            engagement_rate_90d_30d_ago=0.042,
            sound_adoption_lag_avg=6,
            is_trend_dependent_niche=True,
            posts_per_week_90d_avg=4.5
        )
        result = calculate_creator_momentum_score(req)
        assert result.follower_growth_component == 100.0
        assert result.total_score >= 85
    
    def test_declining_creator_scores_low(self):
        req = MomentumScoreRequest(
            creator_id=1,
            follower_growth_90d_pct=-5,
            campaigns_booked_90d=0,
            engagement_rate_90d_current=0.025,
            engagement_rate_90d_30d_ago=0.040,
            sound_adoption_lag_avg=45,
            is_trend_dependent_niche=True,
            posts_per_week_90d_avg=0.8
        )
        result = calculate_creator_momentum_score(req)
        assert result.total_score < 40
```

---

## Implementation Priority

### Phase 1: Foundation (Weeks 1-3)
1. Extend creators table with new performance columns
2. Implement niche_fit_score and momentum_score calculations
3. Add niche health metrics table and quarterly calculation
4. Build CSV export for analytics team

### Phase 2: Integration (Weeks 4-6)
1. Integrate niche fit scoring into campaign matching API
2. Build dashboard for creator tier recommendations
3. Implement crossover detection
4. Add sound trend tracking

### Phase 3: Monitoring (Weeks 7-10)
1. Set up weekly niche health alerts
2. Create admin dashboard for trend tracking
3. Automate quarterly health reports
4. Build creator momentum alerts

---

**Document Status:** Implementation Ready  
**Owner:** Campaign Operations + Engineering  
**Review Date:** September 2026 (post-Phase 3)
