import * as string from "../stdlib/rss/strings.rss";
import { add_one } from "vm";
import * as io from "io";
import * as re from "re";
import * as json from "json";

// Complex JavaScript flavor example: loop + stdlib + host + closure.
let total = 0;
for (let i = 0; i < 4; i = i + 1) {
    total = total + i;
}

if (!string.non_empty("javascript")) {
    total = 0;
} else {
    total = add_one(total);
}

let base = 7;
let add = (value) => value + base;
base = 8;
let closureValue = add(5);

const profile = { stats: { score: closureValue } };
const chainedScore = profile?.stats?.score;
const missingScore = profile?.missing?.value;

function keep(value) { return value; }
const regexOk = re.match("^javascript$", "JAVASCRIPT", "i");
const payload = {
    lang: "javascript",
    score: closureValue,
    chained: chainedScore,
};
const payloadJson = json.encode(payload);
const payloadDecoded = json.decode(payloadJson);
const jsonScore = payloadDecoded.score;
let ioOk = true;
if (false) {
    ioOk = io.exists(".");
}

if (regexOk && ioOk && jsonScore == chainedScore) {
    console.log(keep(chainedScore));
} else {
    console.log(0);
}
