local add_one = require("vm").add_one

local i = 0
local total = 0
while i < 3 do
    total = total + 1
    i = i + 1
end

if total ~= 3 then
    print(0)
elseif total == 3 then
    print(add_one(5))
end
