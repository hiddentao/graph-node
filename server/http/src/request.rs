use graph::serde_json;
use graphql_parser;
use hyper::Chunk;

use graph::components::server::query::GraphQLServerError;
use graph::prelude::*;

/// Future for a query parsed from an HTTP request.
pub struct GraphQLRequest {
    body: Chunk,
    schema: Schema,
}

impl GraphQLRequest {
    /// Creates a new GraphQLRequest future based on an HTTP request and a result sender.
    pub fn new(body: Chunk, schema: Schema) -> Self {
        GraphQLRequest { body, schema }
    }
}

impl Future for GraphQLRequest {
    type Item = Query;
    type Error = GraphQLServerError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        // Fail if no schema is available
        let schema = self.schema.clone();

        // Parse request body as JSON
        let json: serde_json::Value = serde_json::from_slice(&self.body)
            .map_err(|e| GraphQLServerError::ClientError(format!("{}", e)))?;

        // Ensure the JSON data is an object
        let obj = json
            .as_object()
            .ok_or(GraphQLServerError::ClientError(String::from(
                "Request data is not an object",
            )))?;

        // Ensure the JSON data has a "query" field
        let query_value = obj
            .get("query")
            .ok_or(GraphQLServerError::ClientError(String::from(
                "The \"query\" field missing in request data",
            )))?;

        // Ensure the "query" field is a string
        let query_string = query_value.as_str().ok_or(GraphQLServerError::ClientError(
            String::from("The\"query\" field is not a string"),
        ))?;

        // Parse the "query" field of the JSON body
        let document = graphql_parser::parse_query(query_string)
            .map_err(|e| GraphQLServerError::from(QueryError::from(e)))?;

        // Parse the "variables" field of the JSON body, if present
        let variables = match obj.get("variables") {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(variables @ serde_json::Value::Object(_)) => {
                serde_json::from_value(variables.clone())
                    .map_err(|e| GraphQLServerError::ClientError(format!("{}", e)))
                    .map(|v| Some(v))
            }
            _ => Err(GraphQLServerError::ClientError(format!(
                "Invalid query variables provided"
            ))),
        }?;

        Ok(Async::Ready(Query {
            document,
            variables,
            schema,
        }))
    }
}

#[cfg(test)]
mod tests {
    use graphql_parser;
    use hyper;

    use graph::prelude::*;

    use super::GraphQLRequest;

    const EXAMPLE_SCHEMA: &'static str = "type Query { users: [User!] }";

    #[test]
    fn rejects_invalid_json() {
        let schema = Schema {
            name: "test".to_string(),
            id: "test".to_string(),
            document: graphql_parser::parse_schema(EXAMPLE_SCHEMA).unwrap(),
        };
        let request = GraphQLRequest::new(hyper::Chunk::from("!@#)%"), schema);
        request.wait().expect_err("Should reject invalid JSON");
    }

    #[test]
    fn rejects_json_without_query_field() {
        let schema = Schema {
            name: "test".to_string(),
            id: "test".to_string(),
            document: graphql_parser::parse_schema(EXAMPLE_SCHEMA).unwrap(),
        };
        let request = GraphQLRequest::new(hyper::Chunk::from("{}"), schema);
        request
            .wait()
            .expect_err("Should reject JSON without query field");
    }

    #[test]
    fn rejects_json_with_non_string_query_field() {
        let schema = Schema {
            name: "test".to_string(),
            id: "test".to_string(),
            document: graphql_parser::parse_schema(EXAMPLE_SCHEMA).unwrap(),
        };
        let request = GraphQLRequest::new(hyper::Chunk::from("{\"query\": 5}"), schema);
        request
            .wait()
            .expect_err("Should reject JSON with a non-string query field");
    }

    #[test]
    fn rejects_broken_queries() {
        let schema = Schema {
            name: "test".to_string(),
            id: "test".to_string(),
            document: graphql_parser::parse_schema(EXAMPLE_SCHEMA).unwrap(),
        };
        let request = GraphQLRequest::new(hyper::Chunk::from("{\"query\": \"foo\"}"), schema);
        request.wait().expect_err("Should reject broken queries");
    }

    #[test]
    fn accepts_valid_queries() {
        let schema = Schema {
            name: "test".to_string(),
            id: "test".to_string(),
            document: graphql_parser::parse_schema(EXAMPLE_SCHEMA).unwrap(),
        };
        let request = GraphQLRequest::new(
            hyper::Chunk::from("{\"query\": \"{ user { name } }\"}"),
            schema,
        );
        let query = request.wait().expect("Should accept valid queries");
        assert_eq!(
            query.document,
            graphql_parser::parse_query("{ user { name } }").unwrap()
        );
    }

    #[test]
    fn accepts_null_variables() {
        let schema = Schema {
            name: "test".to_string(),
            id: "test".to_string(),
            document: graphql_parser::parse_schema(EXAMPLE_SCHEMA).unwrap(),
        };
        let request = GraphQLRequest::new(
            hyper::Chunk::from(
                "\
                 {\
                 \"query\": \"{ user { name } }\", \
                 \"variables\": null \
                 }",
            ),
            schema,
        );
        let query = request.wait().expect("Should accept null variables");

        let expected_query = graphql_parser::parse_query("{ user { name } }").unwrap();
        assert_eq!(query.document, expected_query);
        assert_eq!(query.variables, None);
    }

    #[test]
    fn rejects_non_map_variables() {
        let schema = Schema {
            name: "test".to_string(),
            id: "test".to_string(),
            document: graphql_parser::parse_schema(EXAMPLE_SCHEMA).unwrap(),
        };
        let request = GraphQLRequest::new(
            hyper::Chunk::from(
                "\
                 {\
                 \"query\": \"{ user { name } }\", \
                 \"variables\": 5 \
                 }",
            ),
            schema,
        );
        request.wait().expect_err("Should reject non-map variables");
    }

    #[test]
    fn parses_variables() {
        let schema = Schema {
            name: "test".to_string(),
            id: "test".to_string(),
            document: graphql_parser::parse_schema(EXAMPLE_SCHEMA).unwrap(),
        };
        let request = GraphQLRequest::new(
            hyper::Chunk::from(
                "\
                 {\
                 \"query\": \"{ user { name } }\", \
                 \"variables\": { \"foo\": \"bar\" } \
                 }",
            ),
            schema,
        );
        let query = request.wait().expect("Should accept valid queries");

        let expected_query = graphql_parser::parse_query("{ user { name } }").unwrap();
        let mut expected_variables = QueryVariables::new();
        expected_variables.insert("foo".to_string(), QueryVariableValue::from("bar"));

        assert_eq!(query.document, expected_query);
        assert_eq!(query.variables, Some(expected_variables));
    }
}
