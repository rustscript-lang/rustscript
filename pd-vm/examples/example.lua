local add_one = require("vm").add_one

local total = 0
for i = 3, 1, -1 do
    total = total + 1
end

if total ~= 3 then
    print(0)
elseif total == 3 then
    print(add_one(5))
end
