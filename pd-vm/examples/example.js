function keep(value) {
    return value;
}

let i = 0;
let total = 0;
while (i < 3) {
    total = total + 1;
    i = i + 1;
}

if (total != 3) {
    console.log(0);
} else {
    console.log(keep(6));
}
