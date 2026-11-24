include "common";
# Resolve a scoped weekly pool only to explicitly configured matching model names.
def scoped($models):
  if (.scope | type) != "object" or .scope.surface != null or (.scope.model | type) != "object"
  then error("unknown usage scope") else . end
  | .scope.model as $scope
  | if ($scope.id != null and ($scope.id | type) != "string")
       or ($scope.display_name != null and ($scope.display_name | type) != "string")
    then error("invalid model scope") else . end
  | ($scope.display_name // "" | tokens) as $wanted
  | (if ($scope.id // "") != "" then $scope.id
     elif ($wanted | length) > 0 then ($wanted | join("-")) else error("empty model scope") end) as $key
  | {pool:("model:"+$key), window:"seven_day", models:[$models[] |
      . as $name | (tokens) as $parts
      | select($name == $scope.id or (($wanted | length) > 0 and (($wanted - $parts) | length) == 0))]};

# Preserve the grant inventory and count only uses the provider marks usable now.
def claude_credits:
  try (
    .cedar_ember
    | if .eligible == false then error("ineligible reset surface") else . end
    | .grants | credit_rows
    | map(
        (.clears // [] | map(credit_scope) | map(select(. != "unknown")) | unique) as $scopes
        | {id:(.id | credit_text(128)), provider_kind:((.type // "GRANT") | credit_text(64)),
           label:((.label // "Banked Reset Credit") | credit_text(128)),
           scope:(if $scopes == ["five_hour","seven_day"] then "all"
                  elif ($scopes | length) == 1 then $scopes[0] else "unknown" end),
           resets_left:(.resets_left | credit_count),
           usable_now:(.usable_now | if type == "boolean" then . else error("invalid usable flag") end),
           granted_at:(.starts_at | credit_time), expires_at:(.ends_at | credit_time)}
      )
    | {known:true, available_count:((map(select(.usable_now).resets_left) | add // 0) | credit_count), credits:.}
  ) catch unknown_credits;

# Only limits[] is authoritative here; parallel summary objects are not added twice.
. as $response |
.limits
| if type != "array" then error("missing usage limits") else . end
| [ .[] | . as $entry
    | (if .kind == "session" or .kind == "weekly_all" then
         if .scope != null then error("unexpected scope") else
           {pool:(if .kind == "session" then "primary" else "secondary" end),
            window:(if .kind == "session" then "five_hour" else "seven_day" end), models:($ctx.models | keys)} end
       elif .kind == "weekly_scoped" then scoped($ctx.models | keys)
       else error("unknown usage kind") end) as $window
    | ($entry.percent | percent) as $used
    | if $entry.is_active != null and ($entry.is_active | type) != "boolean" then error("invalid active flag") else . end
    | ($entry.resets_at | reset) as $reset
    | select(($window.models | length) > 0)
    | $window + {remaining_percent:(100-$used), reset_at:$reset, observed_at:$ctx.now}
  ] | envelope | .reset_credits = ($response | claude_credits)
