local add_one = require("vm").add_one
local string = require("../stdlib/rss/strings.rss")

-- Complex Lua flavor example: loop + stdlib + host + closure.
local total = 0
for i = 0, 3, 1 do
    total = total + i
end

if not string.non_empty("lua") then
    total = 0
else
    total = add_one(total)
end

local base = 7
local add = function(value) return value + base end
base = 8
local closure_value = add(5)

local profile = { stats = { score = closure_value } }
local chained_score = profile?.stats?.score
local missing_score = profile?.missing?.value

print(closure_value)
