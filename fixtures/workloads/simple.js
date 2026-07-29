// Natural tier-up: no natives syntax, warms up through Ignition -> Maglev.
// Smallest possible Maglev graph, used as the baseline parser fixture.
function add(a, b) {
  return a + b;
}

let acc = 0;
for (let i = 0; i < 100000; i++) {
  acc = add(acc, i) | 0;
}
print(acc);
