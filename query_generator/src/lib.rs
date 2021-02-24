#![feature(or_insert_with_key)]

use derive_more::{Display, From, Into};
use itertools::Itertools;
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::iter;
use strum::IntoEnumIterator;
use strum_macros::EnumIter;

use nom_sql::{
    BinaryOperator, Column, ColumnSpecification, ConditionBase, ConditionExpression, ConditionTree,
    CreateTableStatement, FieldDefinitionExpression, FieldValueExpression, FunctionArgument,
    FunctionExpression, ItemPlaceholder, JoinClause, JoinConstraint, JoinOperator, JoinRightSide,
    Literal, LiteralExpression, SelectStatement, SqlType, Table,
};

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, From, Into, Display, Clone)]
#[repr(transparent)]
pub struct TableName(String);

impl From<TableName> for Table {
    fn from(name: TableName) -> Self {
        Table {
            name: name.into(),
            alias: None,
            schema: None,
        }
    }
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, From, Into, Display, Clone)]
#[repr(transparent)]
pub struct ColumnName(String);

impl From<ColumnName> for Column {
    fn from(name: ColumnName) -> Self {
        Self {
            name: name.into(),
            alias: None,
            table: None,
            function: None,
        }
    }
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct TableSpec {
    pub name: TableName,
    pub columns: HashMap<ColumnName, SqlType>,
    column_name_counter: u32,
}

impl From<TableSpec> for CreateTableStatement {
    fn from(spec: TableSpec) -> Self {
        CreateTableStatement {
            table: spec.name.into(),
            fields: spec
                .columns
                .into_iter()
                .map(|(col_name, col_type)| ColumnSpecification {
                    column: col_name.into(),
                    sql_type: col_type,
                    constraints: vec![],
                    comment: None,
                })
                .collect(),
            keys: None,
        }
    }
}

impl TableSpec {
    pub fn new(name: TableName) -> Self {
        Self {
            name,
            columns: Default::default(),
            column_name_counter: 0,
        }
    }

    pub fn fresh_column(&mut self) -> ColumnName {
        self.fresh_column_with_type(SqlType::Int(32))
    }

    pub fn fresh_column_with_type(&mut self, col_type: SqlType) -> ColumnName {
        self.column_name_counter += 1;
        let column_name = ColumnName(format!("column_{}", self.column_name_counter));
        self.columns.insert(column_name.clone(), col_type);
        column_name
    }

    pub fn some_column_name(&mut self) -> ColumnName {
        self.columns
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| self.fresh_column())
    }
}

#[derive(Debug, Default)]
pub struct GeneratorState {
    tables: HashMap<TableName, TableSpec>,
    table_name_counter: u32,
    alias_counter: u32,
}

impl GeneratorState {
    pub fn fresh_table_mut(&mut self) -> &mut TableSpec {
        self.table_name_counter += 1;
        let table_name = TableName(format!("table_{}", self.table_name_counter));
        self.tables
            .entry(table_name)
            .or_insert_with_key(|tn| TableSpec::new(tn.clone()))
    }

    pub fn table_mut<'a, TN>(&'a mut self, name: &TN) -> &'a mut TableSpec
    where
        TableName: Borrow<TN>,
        TN: Eq + Hash,
    {
        self.tables.get_mut(name).unwrap()
    }

    pub fn table_names(&self) -> impl Iterator<Item = &TableName> {
        self.tables.keys()
    }

    pub fn some_table_mut(&mut self) -> &mut TableSpec {
        if self.tables.is_empty() {
            self.fresh_table_mut()
        } else {
            self.tables.values_mut().next().unwrap()
        }
    }

    pub fn new_query(&mut self) -> QueryState<'_> {
        QueryState::new(self)
    }

    pub fn generate_query<'a, I>(&mut self, operations: I) -> SelectStatement
    where
        I: IntoIterator<Item = &'a QueryOperation>,
    {
        let mut query = SelectStatement::default();
        let mut state = self.new_query();
        for op in operations {
            op.add_to_query(&mut state, &mut query);
        }
        for table in state.tables.into_iter() {
            query.tables.push(Table {
                name: table.into(),
                alias: None,
                schema: None,
            })
        }
        query
    }

    pub fn generate_queries(
        &mut self,
        max_depth: usize,
    ) -> impl Iterator<Item = SelectStatement> + '_ {
        QueryOperation::permute(max_depth).map(move |ops| self.generate_query(ops))
    }

    pub fn into_ddl(self) -> impl Iterator<Item = CreateTableStatement> {
        self.tables.into_iter().map(|(_, tbl)| tbl.into())
    }

    pub fn ddl(&self) -> impl Iterator<Item = CreateTableStatement> + '_ {
        self.tables.iter().map(|(_, tbl)| tbl.clone().into())
    }
}

pub struct QueryState<'a> {
    gen: &'a mut GeneratorState,
    tables: HashSet<TableName>,
    alias_counter: u32,
}

impl<'a> QueryState<'a> {
    pub fn new(gen: &'a mut GeneratorState) -> Self {
        Self {
            gen,
            tables: HashSet::new(),
            alias_counter: 0,
        }
    }

    pub fn fresh_alias(&mut self) -> String {
        self.alias_counter += 1;
        format!("alias_{}", self.alias_counter)
    }

    pub fn some_table_mut(&mut self) -> &mut TableSpec {
        let table = self.gen.some_table_mut();
        self.tables.insert(table.name.clone());
        table
    }

    pub fn fresh_table_mut(&mut self) -> &mut TableSpec {
        let table = self.gen.fresh_table_mut();
        self.tables.insert(table.name.clone());
        table
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Copy, EnumIter, Serialize, Deserialize)]
pub enum AggregateType {
    Count,
    Sum,
    Avg,
    GroupConcat,
    Max,
    Min,
}

#[derive(Debug, Eq, PartialEq, Clone, Copy, EnumIter, Serialize, Deserialize)]
pub enum FilterRHS {
    Constant,
    Column,
}

#[derive(Debug, Eq, PartialEq, Clone, Copy, EnumIter, Serialize, Deserialize)]
pub enum LogicalOp {
    And,
    Or,
}

impl From<LogicalOp> for BinaryOperator {
    fn from(op: LogicalOp) -> Self {
        match op {
            LogicalOp::And => BinaryOperator::And,
            LogicalOp::Or => BinaryOperator::Or,
        }
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub struct Filter {
    operator: BinaryOperator,
    rhs: FilterRHS,
    extend_where_with: LogicalOp,
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub enum QueryOperation {
    ColumnAggregate(AggregateType),
    Filter(Filter),
    Distinct,
    Join(JoinOperator),
    ProjectLiteral,
    SingleParameter,
    MultipleParameters,
}

const COMPARISON_OPS: &[BinaryOperator] = &[
    BinaryOperator::Equal,
    BinaryOperator::NotEqual,
    BinaryOperator::Greater,
    BinaryOperator::GreaterOrEqual,
    BinaryOperator::Less,
    BinaryOperator::LessOrEqual,
];

const JOIN_OPERATORS: &[JoinOperator] = &[
    JoinOperator::LeftJoin,
    JoinOperator::LeftOuterJoin,
    JoinOperator::InnerJoin,
];

lazy_static! {
    static ref ALL_FILTERS: Vec<Filter> = {
        COMPARISON_OPS
            .iter()
            .cartesian_product(FilterRHS::iter())
            .cartesian_product(LogicalOp::iter())
            .map(|((operator, rhs), extend_where_with)| Filter {
                operator: *operator,
                rhs,
                extend_where_with,
            })
            .collect()
    };
    static ref ALL_OPERATIONS: Vec<QueryOperation> = {
        AggregateType::iter()
            .map(QueryOperation::ColumnAggregate)
            .chain(ALL_FILTERS.iter().cloned().map(QueryOperation::Filter))
            .chain(iter::once(QueryOperation::Distinct))
            .chain(JOIN_OPERATORS.iter().cloned().map(QueryOperation::Join))
            .chain(iter::once(QueryOperation::ProjectLiteral))
            .chain(iter::once(QueryOperation::SingleParameter))
            .collect()
    };
}

fn extend_where(query: &mut SelectStatement, op: LogicalOp, cond: ConditionExpression) {
    query.where_clause = Some(match query.where_clause.take() {
        Some(existing_cond) => ConditionExpression::LogicalOp(ConditionTree {
            operator: op.into(),
            left: Box::new(existing_cond),
            right: Box::new(cond),
        }),
        None => cond,
    })
}

fn and_where(query: &mut SelectStatement, cond: ConditionExpression) {
    extend_where(query, LogicalOp::And, cond)
}

impl QueryOperation {
    fn add_to_query<'state>(&self, state: &mut QueryState<'state>, query: &mut SelectStatement) {
        match self {
            QueryOperation::ColumnAggregate(agg) => {
                use AggregateType::*;

                let alias = state.fresh_alias();
                let tbl = state.some_table_mut();
                let col = tbl.fresh_column_with_type(match agg {
                    GroupConcat => SqlType::Text,
                    _ => SqlType::Int(32),
                });
                let arg = FunctionArgument::Column(Column {
                    name: col.into(),
                    alias: None,
                    table: Some(tbl.name.clone().into()),
                    function: None,
                });
                let func = match agg {
                    Count => FunctionExpression::Count(arg, false),
                    Sum => FunctionExpression::Sum(arg, false),
                    Avg => FunctionExpression::Avg(arg, false),
                    GroupConcat => FunctionExpression::GroupConcat(arg, ", ".to_owned()),
                    Max => FunctionExpression::Max(arg),
                    Min => FunctionExpression::Min(arg),
                };

                query.fields.push(FieldDefinitionExpression::Col(Column {
                    name: alias.clone(),
                    alias: Some(alias),
                    table: None,
                    function: Some(Box::new(func)),
                }))
            }

            QueryOperation::Filter(filter) => {
                let tbl = state.some_table_mut();
                let col = tbl.fresh_column_with_type(SqlType::Int(1));
                let right = Box::new(match filter.rhs {
                    FilterRHS::Constant => ConditionExpression::Base(ConditionBase::Literal(
                        // TODO(grfn): Tell the generatorstate about all the values we want to exist
                        // per column
                        Literal::Integer(1),
                    )),
                    FilterRHS::Column => {
                        let col = tbl.fresh_column();
                        ConditionExpression::Base(ConditionBase::Field(col.into()))
                    }
                });

                let cond = ConditionExpression::ComparisonOp(ConditionTree {
                    operator: filter.operator,
                    left: Box::new(ConditionExpression::Base(ConditionBase::Field(
                        col.clone().into(),
                    ))),
                    right,
                });

                query
                    .fields
                    .push(FieldDefinitionExpression::Col(col.into()));

                extend_where(query, filter.extend_where_with, cond);
            }

            QueryOperation::Distinct => {
                query.distinct = true;
            }

            QueryOperation::Join(operator) => {
                let left_table = state.some_table_mut();
                let left_join_key = left_table.fresh_column_with_type(SqlType::Int(32));
                let left_projected = left_table.fresh_column();

                let right_table = state.fresh_table_mut();
                let right_join_key = right_table.fresh_column_with_type(SqlType::Int(32));
                let right_projected = right_table.fresh_column();

                query.join.push(JoinClause {
                    operator: *operator,
                    right: JoinRightSide::Table(right_table.name.clone().into()),
                    constraint: JoinConstraint::On(ConditionExpression::ComparisonOp(
                        ConditionTree {
                            operator: BinaryOperator::Equal,
                            left: Box::new(ConditionExpression::Base(ConditionBase::Field(
                                left_join_key.into(),
                            ))),
                            right: Box::new(ConditionExpression::Base(ConditionBase::Field(
                                right_join_key.into(),
                            ))),
                        },
                    )),
                });

                query
                    .fields
                    .push(FieldDefinitionExpression::Col(left_projected.into()));
                query
                    .fields
                    .push(FieldDefinitionExpression::Col(right_projected.into()));
            }
            QueryOperation::ProjectLiteral => {
                query.fields.push(FieldDefinitionExpression::Value(
                    FieldValueExpression::Literal(LiteralExpression {
                        value: Literal::Integer(1),
                        alias: None,
                    }),
                ));
            }
            QueryOperation::SingleParameter => {
                let col = state.some_table_mut().some_column_name();
                and_where(
                    query,
                    ConditionExpression::ComparisonOp(ConditionTree {
                        operator: BinaryOperator::Equal,
                        left: Box::new(ConditionExpression::Base(ConditionBase::Field(col.into()))),
                        right: Box::new(ConditionExpression::Base(ConditionBase::Literal(
                            Literal::Placeholder(ItemPlaceholder::QuestionMark),
                        ))),
                    }),
                )
            }
            QueryOperation::MultipleParameters => {
                QueryOperation::SingleParameter.add_to_query(state, query);
                QueryOperation::SingleParameter.add_to_query(state, query);
            }
        }
    }

    pub fn permute(max_depth: usize) -> impl Iterator<Item = Vec<&'static QueryOperation>> {
        (1..=max_depth).flat_map(|depth| ALL_OPERATIONS.iter().combinations(depth))
    }
}
