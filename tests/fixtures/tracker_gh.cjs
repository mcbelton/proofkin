const fs = require("node:fs");
const args = process.argv.slice(2);
if (args[0] === "auth" && args[1] === "token") {
  console.log("fixture-account");
  process.exit(0);
}
if (args[0] !== "api" || args[1] !== "graphql") throw new Error(`Unexpected arguments: ${args}`);
const { query, variables } = JSON.parse(fs.readFileSync(0, "utf8"));
const name = query.split(" ")[1].split("(")[0];
const repo = `${variables.owner}/${variables.name}`;
const url = `https://github.com/${repo}`;
let data;
switch (name) {
  case "Repo":
    data = {
      viewer: { id: "U1", login: "alice" },
      repository: { id: repo, url, visibility: variables.name === "tasks" ? "PRIVATE" : "PUBLIC" },
    };
    break;
  case "Board":
    data = { repositoryOwner: { projectV2: {
      id: `P${variables.number}`, title: `Board ${variables.number}`,
      url: `https://github.com/users/alice/projects/${variables.number}`,
      fields: { nodes: [{ id: "A1", name: "Agent", dataType: "TEXT" }] },
    } } };
    break;
  case "Issues": {
    const issues = {};
    for (const match of query.matchAll(new RegExp("issue[(]number: ([0-9]+)[)]", "g"))) {
      const number = Number(match[1]);
      issues[`n${number}`] = {
        id: `${repo}#${number}`, title: `${repo} issue ${number}`, url: `${url}/issues/${number}`,
        number, state: "OPEN", body: "<!-- proofkin:run=r1 -->", subIssuesSummary: { total: 1, completed: 1 },
      };
    }
    data = { repository: issues };
    break;
  }
  case "Settle":
    data = Object.fromEntries(Object.keys(variables).map((alias) => [alias, {}]));
    break;
  default:
    throw new Error(`Unexpected query: ${query}`);
}
console.log(JSON.stringify({ data }));
