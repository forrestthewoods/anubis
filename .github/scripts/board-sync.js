// Rebuilds the Anubis Issue Tracker board (user project #8) from issue labels.
//
// Labels are the source of truth; the board is a view. Every run recomputes
// every card from the labels, so runs are idempotent and their order doesn't
// matter. Cards dragged by hand are put back on the next run.
//
// Run by .github/workflows/board-sync.yml through actions/github-script.
// `github` must be authenticated with a classic PAT that has the `project`
// scope (GITHUB_TOKEN can't reach user-owned projects); `labelsApi` is a
// client with issues:write on this repo, used to keep label colors in step.

// Board field -> label prefix -> allowed values (label value -> option name).
const FIELDS = [
  { field: "Kind", prefix: "", values: { bug: "bug", perf: "perf", feature: "feature", design: "design", chore: "chore" } },
  { field: "Severity", prefix: "sev: ", values: { critical: "critical", high: "high", medium: "medium", low: "low", nit: "nit" } },
  { field: "Plan", prefix: "plan: ", values: { yes: "yes", no: "no" } },
  { field: "Complexity", prefix: "cx: ", values: { low: "low", medium: "medium", high: "high" } },
  { field: "Confidence", prefix: "conf: ", values: { high: "high", medium: "medium", low: "low" } },
  { field: "Worth it", prefix: "worth: ", values: { yes: "yes", no: "no" } },
  {
    field: "Status",
    prefix: "status: ",
    values: {
      "needs-triage": "Needs triage",
      "needs-info": "Needs info",
      triaged: "Triaged",
      ready: "Ready",
      "in-progress": "In progress",
      "in-review": "In review",
      parked: "Parked",
    },
  },
];
const DONE = "Done";
const NEEDS_TRIAGE = "Needs triage";

// One color per label group, so the groups read apart in the issue list.
const COLORS = {
  "": "5319e7",
  "sev: ": "b60205",
  "plan: ": "0e8a16",
  "cx: ": "fbca04",
  "conf: ": "1d76db",
  "worth: ": "c5def5",
  "status: ": "bfdadc",
};
const KIND_COLORS = { bug: "d73a4a", perf: "f9d0c4", feature: "a2eeef", design: "d4c5f9", chore: "ededed" };

// Desired board values for one issue, plus problems found in its labels.
function desiredValues(issue) {
  const names = issue.labels.map((l) => (typeof l === "string" ? l : l.name));
  const want = {};
  const errors = [];
  for (const f of FIELDS) {
    const hits = [];
    const before = errors.length;
    for (const name of names) {
      if (f.prefix === "") {
        if (Object.hasOwn(f.values, name)) hits.push(name);
        continue;
      }
      if (!name.startsWith(f.prefix.trim())) continue;
      const value = name.slice(f.prefix.length);
      if (!name.startsWith(f.prefix) || !Object.hasOwn(f.values, value)) {
        errors.push(`#${issue.number}: unknown label "${name}"`);
        continue;
      }
      hits.push(value);
    }
    if (hits.length > 1) errors.push(`#${issue.number}: conflicting ${f.field} labels: ${hits.join(", ")}`);
    // A field with a bad label is left as the board has it until the labels are fixed.
    if (errors.length > before) continue;
    want[f.field] = hits.length === 1 ? f.values[hits[0]] : null;
  }
  if (issue.state === "closed") want.Status = DONE;
  else if (want.Status === null) want.Status = NEEDS_TRIAGE;
  return { want, errors };
}

const PROJECT_QUERY = `
query($login: String!, $number: Int!, $cursor: String) {
  user(login: $login) {
    projectV2(number: $number) {
      id
      fields(first: 50) {
        nodes { ... on ProjectV2SingleSelectField { id name options { id name } } }
      }
      items(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          content { ... on Issue { number repository { nameWithOwner } } }
          fieldValues(first: 20) {
            nodes {
              ... on ProjectV2ItemFieldSingleSelectValue {
                name
                field { ... on ProjectV2SingleSelectField { name } }
              }
            }
          }
        }
      }
    }
  }
}`;

async function loadProject(github, login, number) {
  let cursor = null;
  let project = null;
  const items = new Map(); // issue number -> { id, values: {field: option name} }
  do {
    const res = await github.graphql(PROJECT_QUERY, { login, number, cursor });
    const p = res.user.projectV2;
    if (!project) project = { id: p.id, fields: p.fields.nodes.filter((f) => f && f.options) };
    for (const node of p.items.nodes) {
      if (!node.content || node.content.repository?.nameWithOwner !== process.env.GITHUB_REPOSITORY) continue;
      const values = {};
      for (const v of node.fieldValues.nodes) if (v && v.field && v.field.name) values[v.field.name] = v.name;
      items.set(node.content.number, { id: node.id, values });
    }
    cursor = p.items.pageInfo.hasNextPage ? p.items.pageInfo.endCursor : null;
  } while (cursor);
  return { project, items };
}

// Creates any missing field; reports missing options (renaming Status options
// by hand keeps the history of existing cards, so the script never rewrites them).
async function checkFields(github, project, errors) {
  for (const f of FIELDS) {
    const options = [...new Set(Object.values(f.values).concat(f.field === "Status" ? [DONE] : []))];
    let field = project.fields.find((x) => x.name === f.field);
    if (!field) {
      const res = await github.graphql(
        `mutation($project: ID!, $name: String!, $options: [ProjectV2SingleSelectFieldOptionInput!]!) {
          createProjectV2Field(input: {projectId: $project, dataType: SINGLE_SELECT, name: $name, singleSelectOptions: $options}) {
            projectV2Field { ... on ProjectV2SingleSelectField { id name options { id name } } }
          }
        }`,
        { project: project.id, name: f.field, options: options.map((name) => ({ name, color: "GRAY", description: "" })) },
      );
      field = res.createProjectV2Field.projectV2Field;
      project.fields.push(field);
      console.log(`Created field "${f.field}"`);
    }
    const missing = options.filter((o) => !field.options.some((x) => x.name === o));
    if (missing.length) errors.push(`project field "${f.field}" is missing options: ${missing.map((m) => `"${m}"`).join(", ")}`);
  }
}

async function syncLabels(labelsApi, owner, repo) {
  const existing = await labelsApi.paginate(labelsApi.rest.issues.listLabelsForRepo, { owner, repo, per_page: 100 });
  const byName = new Map(existing.map((l) => [l.name, l]));
  for (const f of FIELDS) {
    for (const value of Object.keys(f.values)) {
      const name = f.prefix + value;
      const color = f.prefix === "" ? KIND_COLORS[value] : COLORS[f.prefix];
      const label = byName.get(name);
      if (!label) await labelsApi.rest.issues.createLabel({ owner, repo, name, color });
      else if (label.color !== color) await labelsApi.rest.issues.updateLabel({ owner, repo, name, color });
    }
  }
}

async function run({ github, labelsApi, context, core }) {
  const login = process.env.PROJECT_OWNER;
  const number = Number(process.env.PROJECT_NUMBER);
  const { owner, repo } = context.repo;
  const errors = [];

  await syncLabels(labelsApi, owner, repo);

  // A reopened issue goes back to Needs triage: drop its old status label.
  if (context.eventName === "issues" && context.payload.action === "reopened") {
    const issue = context.payload.issue;
    for (const l of issue.labels) {
      if (!l.name.startsWith("status:")) continue;
      await labelsApi.rest.issues.removeLabel({ owner, repo, issue_number: issue.number, name: l.name });
    }
  }

  const { project, items } = await loadProject(github, login, number);
  await checkFields(github, project, errors);
  const fieldByName = new Map(project.fields.map((f) => [f.name, f]));

  const issues = (await labelsApi.paginate(labelsApi.rest.issues.listForRepo, { owner, repo, state: "all", per_page: 100 }))
    .filter((i) => !i.pull_request);

  let added = 0;
  let changed = 0;
  for (const issue of issues) {
    const { want, errors: issueErrors } = desiredValues(issue);
    errors.push(...issueErrors);
    let item = items.get(issue.number);
    if (!item) {
      if (issue.state === "closed") continue; // closed before the board knew it
      const res = await github.graphql(
        `mutation($project: ID!, $content: ID!) {
          addProjectV2ItemById(input: {projectId: $project, contentId: $content}) { item { id } }
        }`,
        { project: project.id, content: issue.node_id },
      );
      item = { id: res.addProjectV2ItemById.item.id, values: {} };
      added++;
    }
    for (const [fieldName, value] of Object.entries(want)) {
      const field = fieldByName.get(fieldName);
      if (!field || (item.values[fieldName] ?? null) === value) continue;
      if (value === null) {
        await github.graphql(
          `mutation($project: ID!, $item: ID!, $field: ID!) {
            clearProjectV2ItemFieldValue(input: {projectId: $project, itemId: $item, fieldId: $field}) { clientMutationId }
          }`,
          { project: project.id, item: item.id, field: field.id },
        );
      } else {
        const option = field.options.find((o) => o.name === value);
        if (!option) continue; // already reported by checkFields
        await github.graphql(
          `mutation($project: ID!, $item: ID!, $field: ID!, $option: String!) {
            updateProjectV2ItemFieldValue(input: {projectId: $project, itemId: $item, fieldId: $field, value: {singleSelectOptionId: $option}}) { clientMutationId }
          }`,
          { project: project.id, item: item.id, field: field.id, option: option.id },
        );
      }
      changed++;
    }
  }

  console.log(`${issues.length} issues, ${added} cards added, ${changed} values changed`);
  if (errors.length) core.setFailed(`Board sync found problems:\n${errors.join("\n")}`);
}

module.exports = { run, desiredValues, FIELDS };
