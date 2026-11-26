---
id: PRD-003
type: prd
status: review
owner: "@team-sellers"
review_by: 2027-02-15
summary: Requirements for seller onboarding on Acme Market, from sign-up through identity verification and listing quality checks to a seller's first sale, with targets for each step.
tags: [sellers, onboarding, kyc]
---

# Seller onboarding

A new seller today waits a median of 21 days between creating a shop and making a first sale,
and 35 percent of new shops never sell anything. Some of that is natural, but much of it comes
from onboarding: identity checks that stall for days, listings rejected without a clear reason,
and shops that launch with two blurry photos. This document describes the onboarding we want.
The identity verification service it relies on is proposed in RFC-012, and the payout side, which
onboarding unlocks, is covered by PRD-004.

## Goals {#goals}

- Cut the median time to first sale from 21 days to 10 days, the three-year goal in the vision.
- Approve 80 percent of identity checks automatically within 10 minutes of submission.
- Raise the share of new shops that publish at least five listings in their first week from 38
  percent to 60 percent.
- Keep the share of new shops later removed for fraud or mass-produced goods below 0.5 percent,
  so that faster onboarding does not open the door to bad actors.

Non-goals: a separate onboarding flow for wholesale sellers, and translation of seller tools into
languages beyond the six we already support.

## Identity verification {#kyc}

Every seller must pass a know-your-customer check before they can receive money. Regulations in
the countries where we pay out require us to verify the legal name, date of birth, address and,
for businesses, the registration number and beneficial owners. We separate "can list" from "can
be paid": a seller may publish listings and receive orders as soon as their email and phone are
confirmed, but payouts stay blocked until verification passes.

The check asks for a photo of a government ID and a selfie, compared automatically. Sellers in
14 countries are supported at launch. If the automatic check is inconclusive, the case goes to a
human reviewer with a target decision time of one business day. A seller who fails is told which
document failed and why, and can retry twice before the case is escalated. Verification data is
held by the verification service described in RFC-012, never in the main seller database.

## Listing quality {#listing-quality}

Buyers come to Acme Market for handmade goods, so listing quality is part of the product, not a
moderation afterthought. A listing must have at least three photos of the real item, a title of
no more than 140 characters, a description, materials, a processing time and a shipping profile.
Listings for categories with safety rules, such as children's toys and cosmetics, require the
seller to confirm compliance.

At publication each listing receives a quality score from 0 to 100 based on photo resolution and
variety, description completeness, and the absence of signals of resale, such as stock photos
found elsewhere on the web or identical listings in many shops. Listings below 40 are held for
review; listings between 40 and 70 are published with suggestions shown to the seller. Search uses
the score as a ranking feature, as PRD-002 describes, so improving a listing has a visible reward.

## Time to first sale {#time-to-first-sale}

Time to first sale is the onboarding team's north-star input metric. It is measured from the
moment a shop is created to the first order that is paid and not cancelled within 14 days.

We will shorten it in three ways. First, a guided checklist on the seller dashboard shows the
remaining steps (verification, five listings, shipping profile, shop story) with an estimate of
how much each improves visibility. Second, new shops get a 30-day exposure boost in search,
capped so they never take more than 15 percent of first-page impressions. Third, a seller who
has not sold after 14 days gets a review of their shop with specific suggestions, generated from
the quality score and from what similar successful shops do. Each intervention ships as an
experiment with time to first sale as the primary measure.

## Open questions {#open-questions}

- Should we allow sellers to list before verification in every country, or only where regulation
  clearly permits holding funds for unverified sellers?
- How long may funds wait for a seller who never completes verification before we refund the
  buyers? Legal suggests 90 days; support thinks that is too long for buyers.
- Should the exposure boost for new shops depend on the quality score, so that a shop with poor
  listings does not get free visibility?
- Do we need a lighter verification tier for sellers below a yearly sales threshold, as some
  jurisdictions allow?

These questions block approval of this document; the seller team aims to resolve them at the
next product review.
