/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 *
 * Licensed under the Apache License, Version 2.0 (the "License").
 * You may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

// This module interact with DynamoDB Control Plane APIs
use std::{
    time,
    io::{self, Write, Error as IOError},
};
use ::serde::{Serialize, Deserialize};
use chrono::{DateTime, NaiveDateTime, Utc};
use futures::future::join_all;
use log::{debug, error};
use rusoto_core::Region;
use rusoto_dynamodb::*;
use rusoto_ec2::{Ec2, Ec2Client, DescribeRegionsRequest};

extern crate dialoguer;
use dialoguer::{
    Confirmation,
    theme::ColorfulTheme,
    Select,
};
use tabwriter::TabWriter;

use super::app;


/* =================================================
   struct / enum / const
   ================================================= */

// TableDescription doesn't implement Serialize
// https://docs.rs/rusoto_dynamodb/0.42.0/rusoto_dynamodb/struct.TableDescription.html
#[derive(Serialize, Deserialize, Debug)]
struct PrintDescribeTable {
    name: String,
    region: String,
    status: String,
    schema: PrintPrimaryKeys,

    mode: Mode,
    capacity: Option<PrintCapacityUnits>,

    gsi: Option<Vec<PrintSecondaryIndex>>,
    lsi: Option<Vec<PrintSecondaryIndex>>,

    stream: Option<String>,

    count: i64,
    size_bytes: i64,
    created_at: String,
}

const ONDEMAND_API_SPEC: &'static str = "PAY_PER_REQUEST";

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub enum Mode {
    Provisioned,
    OnDemand,
}

#[derive(Serialize, Deserialize, Debug)]
struct PrintPrimaryKeys {
    pk: String,
    sk: Option<String>
}

#[derive(Serialize, Deserialize, Debug)]
struct PrintCapacityUnits {
    wcu: i64,
    rcu: i64
}

#[derive(Serialize, Deserialize, Debug)]
struct PrintSecondaryIndex {
    name: String,
    schema: PrintPrimaryKeys,
    capacity: Option<PrintCapacityUnits>,
}


/* =================================================
   Public functions
   ================================================= */

pub async fn list_tables_all_regions(cx: app::Context) {
    let ec2 = Ec2Client::new(cx.effective_region());
    let input: DescribeRegionsRequest = DescribeRegionsRequest { ..Default::default() };
    match ec2.describe_regions(input).await {
        Err(e) => { error!("{}", e.to_string()); std::process::exit(1); },
        Ok(res) => {
            join_all(
                res.regions.expect("regions should exist") // Vec<Region>
                   .iter().map(|r|
                        list_tables(cx.clone().with_region(r))
                   )
            ).await;
        },
    };
}


pub async fn list_tables(cx: app::Context) {
    let table_names = list_tables_api(cx.clone()).await;

    println!("DynamoDB tables in region: {}", cx.effective_region().name());
    if table_names.len() == 0 { return println!("  No table in this region."); }

    // if let Some(table_in_config) = cx.clone().config.and_then(|x| x.table) {
    if let Some(table_in_config) = cx.clone().cached_using_table_schema() {
        for table_name in table_names {
            if cx.clone().effective_region().name() == table_in_config.region && table_name == table_in_config.name {
                println!("* {}", table_name);
            } else {
                println!("  {}", table_name);
            }
        }
    } else {
        debug!("No table information (currently using table) is found on config file");
        for table_name in table_names { println!("  {}", table_name) }
    }
}


/// Executed when you call `$ dy desc --all-tables`.
/// Note that `describe_table` function calls are executed in parallel (async + join_all).
pub async fn describe_all_tables(cx: app::Context) {
    let table_names = list_tables_api(cx.clone()).await;
    join_all(table_names.iter().map(|t| describe_table(cx.clone().with_table(t)) )).await;
}


/// Executed when you call `$ dy desc (table)`. Retrieve TableDescription via describe_table_api function,
/// then print them in convenient way using print_table_description function (default/yaml).
pub async fn describe_table(cx: app::Context) {
    debug!("context: {:#?}", &cx);
    let desc: TableDescription = app::describe_table_api(&cx.effective_region(), cx.effective_table_name()).await;
    debug!("Retrieved table to describe is: '{}' table in '{}' region.", &desc.clone().table_name.unwrap(), &cx.effective_region().name());

    // save described table info into cache for future use.
    // Note that when this functiono is called from describe_all_tables, not all tables would be cached as calls are parallel.
    match app::insert_to_table_cache(&cx, desc.clone()) {
        Ok(_) => { debug!("Described table schema was written to the cache file.") },
        Err(e) => println!("Failed to write table schema to the cache with follwoing error: {:?}", e),
    };

    match cx.clone().output.as_ref().map(|x| x.as_str() ) {
        None | Some("yaml") => print_table_description(cx.effective_region(), desc),
        // Some("raw") => println!("{:#?}", desc),
        Some(_) => { println!("ERROR: unsupported output type."); std::process::exit(1); },
    }
}


/// Receives region (just to show in one line for reference) and TableDescription,
/// print them in readable YAML format. NOTE: '~' representes 'null' or 'no value' in YAML syntax.
pub fn print_table_description(region: Region, desc: TableDescription) {
    let attr_defs = desc.clone().attribute_definitions.unwrap();
    let mode = extract_mode(&desc.billing_mode_summary);

    let print_table: PrintDescribeTable = PrintDescribeTable {
        name: String::from(&desc.clone().table_name.unwrap()),
        region: String::from(region.name()),
        status: String::from(&desc.clone().table_status.unwrap()),
        schema: PrintPrimaryKeys {
            pk: app::typed_key("HASH",  &desc).expect("pk should exist").display(),
            sk: app::typed_key("RANGE", &desc).map(|k| k.display()),
        },

        mode: mode.clone(),
        capacity: extract_capacity(&mode, &desc.provisioned_throughput),

        gsi: extract_secondary_indexes(&mode, &attr_defs, desc.global_secondary_indexes),
        lsi: extract_secondary_indexes(&mode, &attr_defs, desc.local_secondary_indexes),
        stream: extract_stream(desc.latest_stream_arn, desc.stream_specification),

        size_bytes: i64::from(desc.table_size_bytes.unwrap()),
        count: i64::from(desc.item_count.unwrap()),
        created_at: epoch_to_rfc3339(desc.creation_date_time.unwrap()),
    };
    println!("{}", serde_yaml::to_string(&print_table).unwrap());
}


/// This function is designed to be called from dynein command, mapped in main.rs.
/// Note that it simply ignores --table option if specified. Newly created table name should be given by the 1st argument "name".
pub async fn create_table(cx: app::Context, name: String, given_keys: Vec<String>) {
    if given_keys.len() == 0 || given_keys.len() > 2 {
        error!("You should pass one or two key definitions with --keys option");
        std::process::exit(1);
    };

    match create_table_api(cx.clone(), name, given_keys).await {
        Ok(desc) => print_table_description(cx.effective_region(), desc),
        Err(e) => {
            debug!("CreateTable API call got an error -- {:#?}", e);
            error!("{}", e.to_string());
            std::process::exit(1);
        },
    }
}


pub async fn create_table_api(cx: app::Context, name: String, given_keys: Vec<String>)
                        -> Result<TableDescription, rusoto_core::RusotoError<rusoto_dynamodb::CreateTableError>> {
    debug!("Trying to create a table '{}' with keys '{:?}'", &name, &given_keys);

    let (key_schema, attribute_definitions) = generate_essential_key_definitions(&given_keys);

    let ddb = DynamoDbClient::new(cx.effective_region());
    let req: CreateTableInput = CreateTableInput {
        table_name: name,
        billing_mode: Some(String::from(ONDEMAND_API_SPEC)),
        key_schema: key_schema, // Vec<KeySchemaElement>
        attribute_definitions: attribute_definitions, // Vec<AttributeDefinition>
        ..Default::default()
    };

    return ddb.create_table(req).await.map(|res| res.table_description.unwrap());
}


pub async fn create_index(cx: app::Context, index_name: String, given_keys: Vec<String>) {
    if given_keys.len() == 0 || given_keys.len() > 2 {
        error!("You should pass one or two key definitions with --keys option");
        std::process::exit(1);
    };
    debug!("Trying to create an index '{}' with keys '{:?}', on table '{}' ", &index_name, &given_keys, &cx.effective_table_name());

    let (key_schema, attribute_definitions) = generate_essential_key_definitions(&given_keys);

    let ddb = DynamoDbClient::new(cx.effective_region());
    let create_gsi_action = CreateGlobalSecondaryIndexAction {
        index_name: index_name,
        key_schema: key_schema,
        projection: Projection { projection_type: Some(String::from("ALL")), non_key_attributes: None, },
        provisioned_throughput: None, // TODO: assign default rcu/wcu if base table is Provisioned mode. currently it works only for OnDemand talbe.
    };
    let gsi_update = GlobalSecondaryIndexUpdate {
        create: Some(create_gsi_action),
        update: None,
        delete: None,
    };
    let req: UpdateTableInput = UpdateTableInput {
        table_name: cx.effective_table_name(),
        attribute_definitions: Some(attribute_definitions), // contains minimum necessary/missing attributes to add to define new GSI.
        global_secondary_index_updates: Some(vec![gsi_update]),
        ..Default::default()
    };

    match ddb.update_table(req).await {
        Err(e) => {
            debug!("UpdateTable API call got an error -- {:#?}", e);
            error!("{}", e.to_string());
            std::process::exit(1);
        },
        Ok(res) => {
            debug!("Returned result: {:#?}", res);
            print_table_description(cx.effective_region(), res.table_description.unwrap());
        }
    }
}


pub async fn delete_table(cx: app::Context, name: String, skip_confirmation: bool) {
    debug!("Trying to delete a table '{}'", &name);

    let msg = format!("You're trying to delete a table '{}'. Are you OK?", &name);
    if !skip_confirmation && !Confirmation::new().with_text(&msg).interact().unwrap() {
        println!("The table delete operation has been canceled.");
        return;
    }

    let ddb = DynamoDbClient::new(cx.effective_region());
    let req: DeleteTableInput = DeleteTableInput { table_name: name, ..Default::default() };

    match ddb.delete_table(req).await {
        Err(e) => {
            debug!("DeleteTable API call got an error -- {:#?}", e);
            error!("{}", e.to_string());
            std::process::exit(1);
        },
        Ok(res) => {
            debug!("Returned result: {:#?}", res);
            println!("DynamoDB table '{}' has been deleted successfully.", res.table_description.unwrap().table_name.unwrap());
        }
    }
}


/// Takes on-demand Backup for the table. It takes --all-tables option but it doesn't take any effect.
///
/// OnDemand backup is a type of backups that can be manually created. Another type is called PITR (Point-In-Time-Restore) but dynein doesn't support it for now.
/// For more information about DynamoDB on-demand backup: https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/BackupRestore.html
pub async fn backup(cx: app::Context, all_tables: bool) {
    // this "backup" function is called only when --list is NOT given. So, --all-tables would be ignored.
    if all_tables { println!("NOTE: --all-tables option is ignored without --list option. Just trying to create a backup for the target table...") };
    debug!("Taking a backof of the table '{}'", cx.effective_table_name());
    let epoch: u64 = time::SystemTime::now().duration_since(time::SystemTime::UNIX_EPOCH)
                     .expect("should be able to generate UNIX EPOCH").as_secs();

    let ddb = DynamoDbClient::new(cx.effective_region());
    let req: CreateBackupInput = CreateBackupInput {
        table_name: cx.effective_table_name(),
        backup_name: format!("{}--dynein-{}", cx.effective_table_name(), epoch),
        ..Default::default()
    };

    debug!("this is the req: {:?}", req);

    match ddb.create_backup(req).await {
        Err(e) => {
            debug!("CreateBackup API call got an error -- {:#?}", e);
            app::bye(1, &e.to_string());
        },
        Ok(res) => {
            debug!("Returned result: {:#?}", res);
            let details = res.backup_details.expect("should have some details");
            println!("Backup creation has been started:");
            println!("  Backup Name: {} (status: {})", details.backup_name, details.backup_status);
            println!("  Backup ARN: {}", details.backup_arn);
            println!("  Backup Size: {} bytes", details.backup_size_bytes.expect("should have table size"));
        }
    }
}


/// List backups for a specified table. With --all-tables option all backups for all tables in the region are shown.
pub async fn list_backups(cx: app::Context, all_tables: bool) -> Result<(), IOError> {
    let backups = list_backups_api(&cx, all_tables).await;
    let mut tw = TabWriter::new(io::stdout());
    // First defining header
    tw.write(((vec![ "Table", "Status", "CreatedAt", "BackupName (size)" ].join("\t")) + "\n").as_bytes())?;
    for backup in backups {
        let line = vec![
            backup.table_name.expect("table name should exist"),
            backup.backup_status.expect("status should exist"),
            epoch_to_rfc3339(backup.backup_creation_date_time.expect("creation date should exist")),
            backup.backup_name.expect("backup name should exist") + &format!(" ({} bytes)", backup.backup_size_bytes.expect("size should exist")),
            String::from("\n")
        ];
        tw.write(line.join("\t").as_bytes())?;
    }
    tw.flush()?;
    Ok(())
}


fn fetch_arn_from_backup_name(backup_name: String, available_backups: Vec<BackupSummary>) -> String {
    available_backups.into_iter().find(|b|
        b.to_owned().backup_name.unwrap() == backup_name
    ) /* Option<BackupSummary */
    .unwrap() /* BackupSummary */
    .backup_arn /* Option<String> */
    .unwrap()
}


/// This function restores DynamoDB table from specified backup data.
/// If you don't specify backup data (name) explicitly, dynein will list backups and you can select out of them.
/// Currently overwriting properties during rstore is not supported.
pub async fn restore(cx: app::Context, backup_name: Option<String>, restore_name: Option<String>) {

    // let backups = list_backups_api(&cx, false).await;
    let available_backups: Vec<BackupSummary> = list_backups_api(&cx, false).await
                                                .into_iter().filter(|b: &BackupSummary|
                                                    b.to_owned()
                                                    .backup_status
                                                    .unwrap() == "AVAILABLE").collect();
    // let available_backups: Vec<BackupSummary> = backups.iter().filter(|b| b.backup_status.to_owned().unwrap() == "AVAILABLE").collect();
    if available_backups.len() == 0 { app::bye(0, "No AVAILABLE state backup found for the table."); };

    let source_table_name = cx.effective_table_name();
    let backup_arn = match backup_name {
        Some(bname) => { fetch_arn_from_backup_name(bname, available_backups) },
        None => {
            let selection_texts: Vec<String> = available_backups.iter().map(|b|
                format!("{} ({}, {} bytes)", 
                    b.to_owned().backup_name.unwrap(),
                    epoch_to_rfc3339(b.backup_creation_date_time.unwrap()),
                    b.backup_size_bytes.unwrap()
                )
            ).collect();

            debug!("available selections: {:#?}", selection_texts);

            let selection = Select::with_theme(&ColorfulTheme::default())
                .with_prompt("Select backup data to restore:")
                .default(0) /* &mut Select */
                .items(&selection_texts[..]) /* &mut Select */
                .interact() /* Result<usize, Error> */
                .unwrap();

            available_backups[selection]
            .backup_arn
            .clone()
            .unwrap()
        },
    };

    let epoch: u64 = time::SystemTime::now().duration_since(time::SystemTime::UNIX_EPOCH)
                     .expect("should be able to generate UNIX EPOCH").as_secs();

    let target_table_name = match restore_name {
        None => format!("{}--restore-{}", source_table_name, epoch),
        Some(restore) => restore,
    };

    let ddb = DynamoDbClient::new(cx.effective_region());
    // https://docs.rs/rusoto_dynamodb/0.44.0/rusoto_dynamodb/struct.RestoreTableFromBackupInput.html
    let req: RestoreTableFromBackupInput = RestoreTableFromBackupInput {
        backup_arn: backup_arn.clone(),
        target_table_name: target_table_name,
        ..Default::default()
    };

    match ddb.restore_table_from_backup(req).await {
        Err(e) => {
            debug!("RestoreTableFromBackup API call got an error -- {:#?}", e);
            /* e.g. ... Possibly see "BackupInUse" error:
                [2020-08-14T13:16:07Z DEBUG dy::control] RestoreTableFromBackup API call got an error -- Service( BackupInUse( "Backup is being used to restore another table: arn:aws:dynamodb:us-west-2:111111111111:table/Music/backup/01527492829107-81b9b3dd",))
            */
        },
        Ok(res) => {
            debug!("Returned result: {:#?}", res);
            println!("Table restoration from: '{}' has been started", &backup_arn);
            let desc = res.table_description.unwrap();
            print_table_description(cx.effective_region(), desc);
        }
    }

}


/* =================================================
   Private functions
   ================================================= */

/// Using Vec of String which is passed via command line,
/// generate KeySchemaElement(s) & AttributeDefinition(s), that are essential information to create DynamoDB tables or GSIs.
fn generate_essential_key_definitions(given_keys: &Vec<String>) -> (Vec<KeySchemaElement>, Vec<AttributeDefinition>) {
    let mut key_schema: Vec<KeySchemaElement> = vec![];
    let mut attribute_definitions: Vec<AttributeDefinition> = vec![];
    let mut key_id = 0;
    for key_str in given_keys {
        let key_and_type = key_str.split(',').collect::<Vec<&str>>();
        if key_and_type.len() > 2 {
            error!("Invalid format for --keys option: '{}'. Valid format is '--keys myPk,S mySk,N'", &key_str);
            std::process::exit(1);
        }

        // assumes first given key is Partition key, and second given key is Sort key (if any).
        key_schema.push(KeySchemaElement {
            attribute_name: String::from(key_and_type[0]),
            key_type: if key_id == 0 { String::from("HASH") } else { String::from("RANGE") },
        });

        // If data type of key is omitted, dynein assumes it as String (S).
        attribute_definitions.push(AttributeDefinition {
            attribute_name: String::from(key_and_type[0]),
            attribute_type: if key_and_type.len() == 2 { String::from(key_and_type[1].to_uppercase()) } else { String::from("S")},
        });

        key_id += 1;
    };
    return (key_schema, attribute_definitions);
}


/// Basically called by list_tables function, which is called from `$ dy list`.
/// To make ListTables API result reusable, separated API logic into this standalone function.
async fn list_tables_api(cx: app::Context) -> Vec<String> {
    let ddb = DynamoDbClient::new(cx.effective_region());
    let req: ListTablesInput = Default::default();
    match ddb.list_tables(req).await {
        Err(e) => {
            debug!("ListTables API call got an error -- {:#?}", e);
            error!("{}", e.to_string());
            std::process::exit(1);
        },
        // ListTables API returns blank array even if no table exists in a region.
        Ok(res)  => res.table_names.expect("This message should not be shown"),
    }
}


/// This function is a private function that simply calls ListBackups API and return results
async fn list_backups_api(cx: &app::Context, all_tables: bool) -> Vec<BackupSummary> {
    let ddb = DynamoDbClient::new(cx.effective_region());
    let req: ListBackupsInput = ListBackupsInput {
        table_name: if all_tables { None } else { Some(cx.effective_table_name())},
        ..Default::default()
    };

    return match ddb.list_backups(req).await {
        Err(e) => {
            debug!("ListBackups API call got an error -- {:#?}", e);
            // app::bye(1, &e.to_string()) // it doesn't meet return value requirement.
            println!("{}", &e.to_string());
            std::process::exit(1);
        },
        Ok(res) => res.backup_summaries.expect("backup result should have something"),
    }
}


fn epoch_to_rfc3339(epoch: f64) -> String {
    let utc_datetime = NaiveDateTime::from_timestamp(epoch as i64, 0);
    return DateTime::<Utc>::from_utc(utc_datetime, Utc).to_rfc3339();
}

pub fn extract_mode(bs: &Option<BillingModeSummary>) -> Mode {
    let provisioned_mode = Mode::Provisioned;
    let ondemand_mode    = Mode::OnDemand;
    match bs {
        // if BillingModeSummary field doesn't exist, the table is Provisioned Mode.
        None => provisioned_mode,
        Some(x) => {
            if x.clone().billing_mode.unwrap() == ONDEMAND_API_SPEC { ondemand_mode }
            else { provisioned_mode }
        },
    }
}

fn extract_capacity(mode: &Mode, cap_desc: &Option<ProvisionedThroughputDescription>)
                    -> Option<PrintCapacityUnits> {
    if mode == &Mode::OnDemand { return None }
    else {
        let desc = cap_desc.as_ref().unwrap();
        return Some(PrintCapacityUnits {
            wcu: desc.write_capacity_units.unwrap(),
            rcu: desc.read_capacity_units.unwrap(),
        })
    }
}

trait IndexDesc {
    fn retrieve_index_name(&self) -> &Option<String>;
    fn retrieve_key_schema(&self) -> &Option<Vec<KeySchemaElement>>;
    fn extract_index_capacity(&self, m: &Mode) -> Option<PrintCapacityUnits>;
}

impl IndexDesc for GlobalSecondaryIndexDescription {
    fn retrieve_index_name(&self) -> &Option<String> { return &self.index_name; }
    fn retrieve_key_schema(&self) -> &Option<Vec<KeySchemaElement>> { return &self.key_schema; }
    fn extract_index_capacity(&self, m: &Mode) -> Option<PrintCapacityUnits> {
        if m == &Mode::OnDemand { return None }
        else { return extract_capacity(m, &self.provisioned_throughput); }
    }
}

impl IndexDesc for LocalSecondaryIndexDescription {
    fn retrieve_index_name(&self) -> &Option<String> { return &self.index_name; }
    fn retrieve_key_schema(&self) -> &Option<Vec<KeySchemaElement>> { return &self.key_schema; }
    fn extract_index_capacity(&self, _: &Mode) -> Option<PrintCapacityUnits> {
        return None; // Unlike GSI, LSI doesn't have it's own capacity.
    }
}

// FYI: https://grammarist.com/usage/indexes-indices/
fn extract_secondary_indexes<T: IndexDesc>(
    mode: &Mode,
    attr_defs: &Vec<AttributeDefinition>,
    indexes: Option<Vec<T>>
) -> Option<Vec<PrintSecondaryIndex>> {
    if indexes.is_none() { return None }
    else {
        let mut xs = Vec::<PrintSecondaryIndex>::new();
        for idx in &indexes.unwrap() {
            let ks = &idx.retrieve_key_schema().as_ref().unwrap();
            let idx = PrintSecondaryIndex {
                name: String::from(idx.retrieve_index_name().as_ref().unwrap()),
                schema: PrintPrimaryKeys {
                    pk: app::typed_key_for_schema("HASH", &ks, &attr_defs).expect("pk should exist").display(),
                    sk: app::typed_key_for_schema("RANGE", &ks, &attr_defs).map(|k| k.display()),
                },
                capacity: idx.extract_index_capacity(mode),
            };
            xs.push(idx);
        }
        return Some(xs);
    }
}

fn extract_stream(arn: Option<String>, spec: Option<StreamSpecification>) -> Option<String> {
    if arn.is_none() { return None }
    else { return Some(format!("{} ({})", arn.unwrap(),
                                          spec.unwrap().stream_view_type.unwrap())); }
}