---
id: define-schema
title: Define schema
workstream: "0001"
kind: task
depends_on: []
gated: false
touches:
  - crates/makina-core/src/plan.rs
status: planned
merged_as: ""
---
# Define schema

Build the typed task-document boundary.

**Steps:**

1. Parse and validate the document.
2. Render canonical frontmatter.

- **Done when:** parsing and rendering preserve the Markdown body exactly.
