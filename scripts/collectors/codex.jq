include "common";
# Name a native quota window consistently with previously stored observations.
def window_name:
  if . == 300 then "five_hour" elif . == 10080 then "seven_day" else "min\(.)" end;

# The provider count can describe an inventory even when no rows are returned.
def codex_credits:
  try (
    (.rateLimitResetCredits // .rate_limit_reset_credits) as $inventory
    | ($inventory.availableCount // $inventory.available_count
       | if . == null then null else credit_count end) as $count
    | ($inventory.credits | if . == null and $count != null then [] else credit_rows end)
    | map(
        (.resetType // .reset_type // "unknown") as $kind
        | (.status // .state // "" | ascii_downcase) as $status
        | {id:((.id // .creditId // .credit_id) | credit_text(128)),
           provider_kind:($kind | credit_text(64)), scope:($kind | credit_scope),
           label:((.title // "Banked Reset Credit") | credit_text(128)), resets_left:null,
           usable_now:(if $status == "available" then true
                       elif (["pending","consumed","redeeming","redeemed","used","expired","unavailable"] | index($status)) != null then false
                       else null end),
           granted_at:((.grantedAt // .granted_at) | credit_time),
           expires_at:((.expiresAt // .expires_at) | credit_time)}
      )
    | {known:true, available_count:($count //
        (if all(.[]; .usable_now != null) then map(select(.usable_now)) | length else null end)), credits:.}
  ) catch unknown_credits;

# Dedicated model buckets constrain only exact token-matching native model names.
(.result // .) as $response
| (if ($response.rateLimitsByLimitId | type) == "object" then $response.rateLimitsByLimitId
   elif ($response.rateLimits | type) == "object" then
     {($response.rateLimits.limitId // "codex"): $response.rateLimits}
   else error("missing rate-limit buckets") end) as $buckets
| ($ctx.models | keys) as $models
| [ $buckets | to_entries[] | select(.key != "codex") | . as $bucket
    | (.value.limitName // .key | tokens) as $name
    | {id:.key, models:[$models[] | select((tokens) == $name)]}
    | select((.models | length) > 0)
  ] as $dedicated
| ($dedicated | map(.models[]) | unique) as $scoped
| ($dedicated + (if $buckets | has("codex") then [{id:"codex",models:($models-$scoped)}] else [] end))
| [ .[] | select((.models | length) > 0) | . as $membership
    | $buckets[.id] | .primary, .secondary | select(. != null)
    | (.usedPercent | percent) as $used
    | if (.windowDurationMins | type) != "number" or .windowDurationMins <= 0
      then error("invalid window duration") else . end
    | if .resetsAt != null and ((.resetsAt | type) != "number" or .resetsAt < 0 or .resetsAt > 253402300799)
      then error("invalid reset") else . end
    | {pool:$membership.id, window:(.windowDurationMins | window_name), models:$membership.models,
       remaining_percent:(100-$used), reset_at:.resetsAt, observed_at:$ctx.now}
  ] | envelope | .reset_credits = ($response | codex_credits)
