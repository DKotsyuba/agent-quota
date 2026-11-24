include "common";

# z.ai dashboard dates can omit the timezone. Its dashboard uses UTC.
def glm_time:
  if type == "string" then
    gsub("^\\s+|\\s+$"; "") | sub(" "; "T")
    | if test("[0-9]T[0-9]{2}:[0-9]{2}:[0-9]{2}(\\.[0-9]+)?$") then . + "Z" else . end
  else . end | credit_time;

# Read one card, tolerating the dashboard's alternate field names.
def glm_card($scope):
  (try (.expireTime // .expiredTime // .expiresAt // .endTime | glm_time) catch null) as $expires
  | (.status // .state // .outcome // .result // .code
     | if type == "string" then ascii_downcase | gsub("[^a-z0-9]"; "") else "" end) as $status
  | if has("available") and .available != null and (.available | type) != "boolean"
    then error("invalid card availability") else . end
  | {id:([.recordId,.id,.packageResetId,.resetId] | map(select(type == "number" or (type == "string" and test("\\S"))))
          | first // error("missing card id") | tostring | credit_text(128)),
     provider_kind:((.grantType // "unknown") | credit_text(64)),
     label:((.packageName // .name // .title // "Banked Reset Credit") | credit_text(128)),
     scope:(.resetType // .type | credit_scope | if . == "five_hour" or . == "seven_day" then . else $scope end),
     resets_left:null, granted_at:null, expires_at:$expires,
     usable_now:(.available != false and .consumed != true and .redeemed != true
                 and (["consumed","redeeming","redeemed","used","expired","unavailable"] | index($status)) == null
                 and ($expires == null or $expires > $ctx.now))};

# The first input contains validated windows. A failed or unrecognized
# secondary response becomes unknown without invalidating those windows.
.[1] as $cards | .[0] | .reset_credits = (
  try (
    $cards
    | if .success == true and (.code == 0 or .code == 200) then .data
      else error("invalid reset card envelope") end
    | ([.fiveHourResets | credit_rows[] | glm_card("five_hour")]
       + [.weekResets | credit_rows[] | glm_card("seven_day")])
    | credit_rows
    | {known:true, available_count:(map(select(.usable_now)) | length), credits:.}
  ) catch unknown_credits
)
