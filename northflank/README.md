# FastStats production on Northflank

`production.template.json` is the production source of truth for both Rust
services and their Aiven Kafka topics, application users, and ACLs. Northflank
builds each service from its Dockerfile, deploys it, and injects generated Aiven
credentials through restricted secret groups.

The template targets an existing **Aiven Free Kafka** service and stays within
its fixed limits:

| Topic | Partitions | Retention | Maximum message |
| --- | ---: | ---: | ---: |
| `web-events-v1` | 2 | 3 days (Aiven fixed) | Aiven default |
| `mods-events-v1` | 2 | 3 days (Aiven fixed) | Aiven default |
| `error-occurrences-v1` | 2 | 3 days (Aiven fixed) | Aiven default |
| `web-vitals-v1` | 2 | 3 days (Aiven fixed) | Aiven default |
| `replay-snapshot` | 2 | 3 days (Aiven fixed) | Aiven default |

The collector gets its own Aiven user with write access to all five topics. The
replay consumer gets a different user with read access only to
`replay-snapshot`. Aiven topic read ACLs also grant consumer-group access.

The same OpenTofu step registers JSON Schema subjects named after each collector topic with
the conventional `-value` suffix. Global and per-subject compatibility are both
`BACKWARD_TRANSITIVE`. The existing Aiven Kafka service must have Karapace Schema Registry
enabled before the workflow is applied. These schemas govern compatibility and document the
plain JSON contract; collector messages are not Confluent-framed and do not contain a registry
schema ID.

The workflow is sequential: Kafka plans are approved and applied before the
services are reconciled.

## Northflank setup

1. The template uses the existing free Aiven Kafka service
   `trying-out-kafka` in project `faststats`.
2. In Northflank account settings, create a cloud provider integration. Select **Aiven**,
   enable the **OpenTofu** feature, and enter an Aiven automation API token.
   The Aiven identity should be restricted to the project containing Kafka.
3. In Northflank, create a template and enable GitOps for this repository. Set
   the file path to `/northflank/production.template.json` and the deployment
   branch to `main`.
4. Keep autorun enabled if merged changes should immediately produce a plan.
   Each OpenTofu node has `usePlan: true`, so Northflank waits for approval
   before applying it.

Do not change any `stateKey`: Northflank uses them to associate future runs with
stored OpenTofu state.

## Runtime environments

The existing `basic-data-collection` secret group is the base environment for
both `collector` and `replay-consumer`. It remains externally managed rather
than being declared by this template, avoiding a duplicate resource. In
Northflank, ensure its restrictions include both service IDs so they receive its
database, Tinybird, object-storage, and other shared variables. None of those
values are duplicated in this repository.

The free service uses Aiven's fixed broker configuration. Large replay records
that exceed its broker message limit will be rejected; upgrading the service
and restoring explicit topic message-size configuration is required before
relying on the application's 17 MiB client-side maximum.

The Aiven API token never reaches either Rust process. It remains in the
Northflank Aiven integration and is used only by OpenTofu. Generated Kafka
passwords are marked as sensitive outputs and flow directly into the matching
secret group.

The credential secret-group nodes wait for the preceding OpenTofu actions to
complete, so their Aiven-generated password outputs exist before Northflank
resolves the references.

The service nodes run before their dedicated Kafka secret groups. Each secret
group restriction uses the service node's resolved ID, so initial bootstrap
works even when the services do not exist yet.

## Services

The template reconciles `collector` and `replay-consumer` as combined services.
Both build from the linked GitHub repository's `replay-kafka` branch. The
collector exposes port 8080 publicly and port 9091 internally for metrics; the
consumer has no ports. These names intentionally match the existing Northflank
service IDs so the template updates them rather than creating duplicates.

## Existing Aiven resources

A new Northflank OpenTofu node starts with empty state and cannot automatically
adopt a topic, user, or ACL that was created manually. Before the first run,
check whether any resource declared by the template already exists. Do not
approve a plan that attempts to recreate it. Migrate the resource into the
node's managed state with Northflank support, rename the new application users,
or schedule a deliberate migration in Aiven. Topic import IDs use
`faststats/trying-out-kafka/TOPIC_NAME`; user IDs use
`faststats/trying-out-kafka/USERNAME`; schema subject IDs use
`faststats/trying-out-kafka/SUBJECT_NAME`. The singleton schema configuration uses
`faststats/trying-out-kafka`.

## Changes and deletion safety

Edit the resource specifications in the template and merge to `replay-kafka`.
OpenTofu plans apply automatically so GitOps runs do not pause for manual plan
approval.
The free tier permits at most five topics and two partitions per topic, and
requires replication factor 2. It has fixed three-day retention, limited
throughput, idle shutdown, and no SLA. Do not increase these partition counts
while using it. This template deliberately has no teardown
workflow, so deleting the Northflank template does not run an OpenTofu destroy
operation against production resources.
