# Quarterly revenue review

_Q3 2026, all regions_

- **Prepared by:** Finance analytics
- **Generated:** 2026-11-18T09:30:00Z
- **Notebook:** nb\_9f2c
- **Version:** sha256:4be1c0ffee
- **Data sources:** Warehouse · analytics · PostgreSQL · Production
- **Parameters:** quarter = 2026-Q3

## Summary

**Revenue:** $46,200 (▲ +5.0% vs last quarter) · On track

**Refunds:** $1,250 (▲ +38.9% vs last quarter) · Off track

Revenue grew **5%** quarter over quarter. See [the dashboard](<https://sqlly.app/docs>) and `orders` table.

- West led growth
- East was flat

| Region | Share |
| --- | ---: |
| West | 41% |

> **Fact**
>
> September West revenue was 900 above trend.
>
> _Evidence: run\_42_

> **Hypothesis** · Promo effect
>
> The spike lines up with the _autumn promo_.
>

### Trend

![line chart](assets/quarterly-revenue-review-line-chart.svg)

**Revenue by month**

| month | region | revenue | orders | low | high |
| --- | --- | ---: | ---: | ---: | ---: |
| 2026-01-01 | West | $1,085.50 | 43 | 976.95 | 1194.05 |
| 2026-01-01 | East | $1,171.00 | 48 | 1053.9 | 1288.1 |
| 2026-01-01 | North | $1,256.50 | 53 | 1130.85 | 1382.15 |
| 2026-02-01 | West | $1,171.00 | 46 | 1053.9 | 1288.1 |
| 2026-02-01 | East | $1,342.00 | 51 | 1207.8 | 1476.2 |
| 2026-02-01 | North | $1,513.00 | 56 | 1361.7 | 1664.3 |
| 2026-03-01 | West | $1,256.50 | 49 | 1130.85 | 1382.15 |
| 2026-03-01 | East | $1,513.00 | 54 | 1361.7 | 1664.3 |
| 2026-03-01 | North | $1,769.50 | 59 | 1592.55 | 1946.45 |
| 2026-04-01 | West | $1,342.00 | 52 | 1207.8 | 1476.2 |
| 2026-04-01 | East | $1,684.00 | 57 | 1515.6 | 1852.4 |
| 2026-04-01 | North | $2,026.00 | 62 | 1823.4 | 2228.6 |
| 2026-05-01 | West | $1,427.50 | 55 | 1284.75 | 1570.25 |
| 2026-05-01 | East | $1,855.00 | 60 | 1669.5 | 2040.5 |
| 2026-05-01 | North | $2,282.50 | 65 | 2054.25 | 2510.75 |
| 2026-06-01 | West | $1,513.00 | 41 | 1361.7 | 1664.3 |
| 2026-06-01 | East | $2,026.00 | 46 | 1823.4 | 2228.6 |
| 2026-06-01 | North | $2,539.00 | 51 | 2285.1 | 2792.9 |
| 2026-07-01 | West | $1,598.50 | 44 | 1438.65 | 1758.35 |
| 2026-07-01 | East | $2,197.00 | 49 | 1977.3 | 2416.7 |
| 2026-07-01 | North | $2,795.50 | 54 | 2515.95 | 3075.05 |
| 2026-08-01 | West | $1,684.00 | 47 | 1515.6 | 1852.4 |
| 2026-08-01 | East | $2,368.00 | 52 | 2131.2 | 2604.8 |
| 2026-08-01 | North | $3,052.00 | 57 | 2746.8 | 3357.2 |
| 2026-09-01 | West | $2,669.50 | 50 | 2402.55 | 2936.45 |
| 2026-09-01 | East | $2,539.00 | 55 | 2285.1 | 2792.9 |
| 2026-09-01 | North | $3,308.50 | 60 | 2977.65 | 3639.35 |
| 2026-10-01 | West | $1,855.00 | 53 | 1669.5 | 2040.5 |
| 2026-10-01 | East | $2,710.00 | 58 | 2439 | 2981 |
| 2026-10-01 | North | $3,565.00 | 63 | 3208.5 | 3921.5 |
| 2026-11-01 | West | $1,940.50 | 56 | 1746.45 | 2134.55 |
| 2026-11-01 | East | $2,881.00 | 61 | 2592.9 | 3169.1 |
| 2026-11-01 | North | $3,821.50 | 66 | 3439.35 | 4203.65 |
| 2026-12-01 | West | $2,026.00 | 42 | 1823.4 | 2228.6 |
| 2026-12-01 | East | $3,052.00 | 47 | 2746.8 | 3357.2 |
| 2026-12-01 | North | $4,078.00 | 52 | 3670.2 | 4485.8 |

- **Passed** No negative revenue · Expected: 0 rows · Actual: 0 rows

- **Failed** Row count within range · Expected: 30..40 · Actual: 36 · Late partition

---

## Details

**Typed values** _(Truncated at 2 rows)_

| sku | amount | big\_id | huge | shipped | at | ok | ratio | note | blob |
| --- | ---: | ---: | ---: | --- | --- | --- | ---: | --- | --- |
| 00123 | 12345.6789012345 | 9007199254740993 | 123456789012345678901234567890 | 2026-11-18 | 2026-11-18 14:30:15 | true | 25.6% | \=HYPERLINK("http://evil.example","click") | 0xDEAD |
| A-7 | \-0.0000000001 | \-42 | 7 |  | not a date | false | NaN | \+1 call me |  |

_Showing 2 of 50000 rows_

**c\_revenue01**

```sql
SELECT month, region, sum(revenue) AS revenue
FROM sales
WHERE quarter = :quarter
GROUP BY 1, 2;
```

- Cell: `c_revenue01`
- Run: `run_42`
- Connection: `Warehouse`
- Database: `analytics`
- Environment: `Production`
- Extracted: `2026-11-18T09:29:58Z`
- SQL hash: `9a0f3b`
- Rows: `36`

![Logo](assets/quarterly-revenue-review-brand-mark.png)
