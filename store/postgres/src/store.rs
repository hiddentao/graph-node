use diesel::dsl::sql;
use diesel::pg::Pg;
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_types::Text;
use diesel::{debug_query, delete, insert_into, result, select};
use filter::store_filter;
use futures::sync::mpsc::{channel, Sender};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use uuid::Uuid;

use graph::components::store::{EventSource, Store as StoreTrait};
use graph::prelude::*;
use graph::serde_json;
use graph::{tokio, tokio::timer::Interval};

use entity_changes::EntityChangeListener;
use functions::{revert_block, set_config};

embed_migrations!("./migrations");

/// Internal representation of a Store subscription.
struct Subscription {
    pub entities: Vec<SubgraphEntityPair>,
    pub sender: Sender<EntityChange>,
}

/// Run all initial schema migrations.
///
/// Creates the "entities" table if it doesn't already exist.
fn initiate_schema(logger: &slog::Logger, conn: &PgConnection) {
    // Collect migration logging output
    let mut output = vec![];

    match embedded_migrations::run_with_output(conn, &mut output) {
        Ok(_) => info!(logger, "Completed pending Postgres schema migrations"),
        Err(e) => panic!("Error with Postgres schema setup: {:?}", e),
    }

    // If there was any migration output, log it now
    if !output.is_empty() {
        debug!(logger, "Postgres migration output";
               "output" => String::from_utf8(output).unwrap_or(String::from("<unreadable>")));
    }
}

/// Configuration for the Diesel/Postgres store.
pub struct StoreConfig {
    pub url: String,
}

/// A Store based on Diesel and Postgres.
pub struct Store {
    logger: slog::Logger,
    subscriptions: Arc<RwLock<HashMap<String, Subscription>>>,
    change_listener: EntityChangeListener,
    pub conn: PgConnection,
}

impl Store {
    pub fn new(config: StoreConfig, logger: &slog::Logger) -> Self {
        // Create a store-specific logger
        let logger = logger.new(o!("component" => "Store"));

        // Connect to Postgres
        let conn =
            PgConnection::establish(config.url.as_str()).expect("failed to connect to Postgres");

        info!(logger, "Connected to Postgres"; "url" => &config.url);

        // Create the entities table (if necessary)
        initiate_schema(&logger, &conn);

        // Listen to entity changes in Postgres
        let mut change_listener = EntityChangeListener::new(config.url.clone());
        let entity_changes = change_listener
            .take_event_stream()
            .expect("Failed to listen to entity change events in Postgres");

        // Create the store
        let mut store = Store {
            logger: logger.clone(),
            subscriptions: Arc::new(RwLock::new(HashMap::new())),
            change_listener,
            conn: conn,
        };

        // Deal with store subscriptions
        store.handle_entity_changes(entity_changes);
        store.periodically_clean_up_stale_subscriptions();

        // We're ready for processing entity changes
        store.change_listener.start();

        // Return the store
        store
    }

    /// Handles entity changes emitted by Postgres.
    fn handle_entity_changes(
        &mut self,
        entity_changes: Box<Stream<Item = EntityChange, Error = ()> + Send>,
    ) {
        let logger = self.logger.clone();
        let subscriptions = self.subscriptions.clone();

        tokio::spawn(entity_changes.for_each(move |change| {
            debug!(logger, "Entity change";
                           "subgraph" => &change.subgraph,
                           "entity" => &change.entity,
                           "id" => &change.id);

            // Obtain IDs and senders of subscriptions matching the entity change
            let matches = subscriptions
                .read()
                .unwrap()
                .iter()
                .filter(|(_, subscription)| {
                    subscription
                        .entities
                        .contains(&(change.subgraph.clone(), change.entity.clone()))
                })
                .map(|(id, subscription)| (id.clone(), subscription.sender.clone()))
                .collect::<Vec<_>>();

            let subscriptions = subscriptions.clone();
            let logger = logger.clone();

            // Write change to all matching subscription streams; remove subscriptions
            // whose receiving end has been dropped
            stream::iter_ok::<_, ()>(matches).for_each(move |(id, sender)| {
                let logger = logger.clone();
                let subscriptions = subscriptions.clone();
                sender
                    .send(change.clone())
                    .map_err(move |_| {
                        debug!(logger, "Unsubscribe"; "id" => &id);
                        subscriptions.write().unwrap().remove(&id);
                    })
                    .and_then(|_| Ok(()))
            })
        }));
    }

    fn periodically_clean_up_stale_subscriptions(&mut self) {
        let logger = self.logger.clone();
        let subscriptions = self.subscriptions.clone();

        // Clean up stale subscriptions every 5s
        tokio::spawn(
            Interval::new(Instant::now(), Duration::from_secs(5))
                .for_each(move |_| {
                    let mut subscriptions = subscriptions.write().unwrap();

                    // Obtain IDs of subscriptions whose receiving end has gone
                    let stale_ids = subscriptions
                        .iter_mut()
                        .filter_map(
                            |(id, subscription)| match subscription.sender.poll_ready() {
                                Err(_) => Some(id.clone()),
                                _ => None,
                            },
                        )
                        .collect::<Vec<_>>();

                    // Remove all stale subscriptions
                    for id in stale_ids {
                        debug!(logger, "Unsubscribe"; "id" => &id);
                        subscriptions.remove(&id);
                    }

                    Ok(())
                })
                .map_err(|_| unreachable!()),
        );
    }

    /// Handles block reorganizations.
    /// Revert all store events related to the given block
    pub fn revert_events(&self, block_hash: String) {
        select(revert_block(block_hash))
            .execute(&self.conn)
            .unwrap();
    }
}

impl BasicStore for Store {
    fn get(&self, key: StoreKey) -> Result<Entity, ()> {
        debug!(self.logger, "get"; "key" => format!("{:?}", key));

        use db_schema::entities::dsl::*;

        // Use primary key fields to get the entity; deserialize the result JSON
        entities
            .find((key.id, key.subgraph, key.entity))
            .select(data)
            .first::<serde_json::Value>(&self.conn)
            .map(|value| {
                serde_json::from_value::<Entity>(value).expect("Failed to deserialize entity")
            })
            .map_err(|_| ())
    }

    fn set(
        &mut self,
        key: StoreKey,
        input_entity: Entity,
        input_event_source: EventSource,
    ) -> Result<(), ()> {
        debug!(self.logger, "set"; "key" => format!("{:?}", key));

        use db_schema::entities::dsl::*;

        // Update the existing entity, if necessary
        let updated_entity = match self.get(key.clone()) {
            Ok(mut existing_entity) => {
                existing_entity.merge(input_entity);
                existing_entity
            }
            Err(_) => input_entity,
        };

        // Convert Entity hashmap to serde_json::Value for insert
        let entity_json: serde_json::Value =
            serde_json::to_value(&updated_entity).expect("Failed to serialize entity");

        // Insert entity, perform an update in case of a primary key conflict
        insert_into(entities)
            .values((
                id.eq(&key.id),
                entity.eq(&key.entity),
                subgraph.eq(&key.subgraph),
                data.eq(&entity_json),
                event_source.eq(&input_event_source.to_string()),
            ))
            .on_conflict((id, entity, subgraph))
            .do_update()
            .set((
                id.eq(&key.id),
                entity.eq(&key.entity),
                subgraph.eq(&key.subgraph),
                data.eq(&entity_json),
                event_source.eq(&input_event_source.to_string()),
            ))
            .execute(&self.conn)
            .map(|_| ())
            .map_err(|_| ())
    }

    fn delete(&mut self, key: StoreKey, input_event_source: EventSource) -> Result<(), ()> {
        debug!(self.logger, "delete"; "key" => format!("{:?}", key));

        use db_schema::entities::dsl::*;

        self.conn
            .transaction::<usize, result::Error, _>(|| {
                // Set session variable to store the source of the event
                select(set_config(
                    "vars.current_event_source",
                    input_event_source.to_string(),
                    false,
                )).execute(&self.conn)
                    .unwrap();

                // Delete from DB where rows match the subgraph ID, entity name and ID
                delete(
                    entities
                        .filter(subgraph.eq(&key.subgraph))
                        .filter(entity.eq(&key.entity))
                        .filter(id.eq(&key.id)),
                ).execute(&self.conn)
            })
            .map(|_| ())
            .map_err(|_| ())
    }

    fn find(&self, query: StoreQuery) -> Result<Vec<Entity>, ()> {
        use db_schema::entities::dsl::*;

        // Create base boxed query; this will be added to based on the
        // query parameters provided
        let mut diesel_query = entities
            .filter(entity.eq(query.entity))
            .filter(subgraph.eq(query.subgraph))
            .select(data)
            .into_boxed::<Pg>();

        // Add specified filter to query
        if let Some(filter) = query.filter {
            diesel_query = store_filter(diesel_query, filter).map_err(|e| {
                error!(self.logger, "value does not support this filter";
                                    "value" => format!("{:?}", e.value),
                                    "filter" => e.filter)
            })?;
        }

        // Add order by filters to query
        if let Some(order_attribute) = query.order_by {
            let direction = query
                .order_direction
                .map(|direction| match direction {
                    StoreOrder::Ascending => String::from("ASC"),
                    StoreOrder::Descending => String::from("DESC"),
                })
                .unwrap_or(String::from("ASC"));

            diesel_query = diesel_query.order(
                sql::<Text>("data ->> ")
                    .bind::<Text, _>(order_attribute)
                    .sql(&format!(" {} ", direction)),
            )
        }

        // Add range filter to query
        if let Some(range) = query.range {
            diesel_query = diesel_query
                .limit(range.first as i64)
                .offset(range.skip as i64);
        }

        debug!(self.logger, "find";
                "sql" => format!("{:?}", debug_query::<Pg, _>(&diesel_query)));

        // Process results; deserialize JSON data
        diesel_query
            .load::<serde_json::Value>(&self.conn)
            .map(|values| {
                values
                    .into_iter()
                    .map(|value| {
                        serde_json::from_value::<Entity>(value)
                            .expect("Error to deserialize entity")
                    })
                    .collect()
            })
            .map_err(|_| ())
    }
}

impl StoreTrait for Store {
    fn subscribe(&mut self, entities: Vec<SubgraphEntityPair>) -> EntityChangeStream {
        let subscriptions = self.subscriptions.clone();

        // Generate a new (unique) UUID; we're looping just to be sure we avoid collisions
        let mut id = Uuid::new_v4().to_string();
        while subscriptions.read().unwrap().contains_key(&id) {
            id = Uuid::new_v4().to_string();
        }

        debug!(self.logger, "Subscribe";
               "id" => &id,
               "entities" => format!("{:?}", entities));

        // Prepare the new subscription by creating a channel and a subscription object
        let (sender, receiver) = channel(100);
        let subscription = Subscription { entities, sender };

        // Add the new subscription
        let mut subscriptions = subscriptions.write().unwrap();
        subscriptions.insert(id, subscription);

        // Return the subscription ID and entity change stream
        Box::new(receiver)
    }
}
