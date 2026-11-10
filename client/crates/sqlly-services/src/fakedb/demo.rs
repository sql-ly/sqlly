//! The built-in demo dataset: a small Northwind-flavored catalog with a few
//! tables, views, and stored procedures — enough to demonstrate schema
//! browsing, querying, and procedure execution without any real database.

use crate::fakedb::{Column, FakeDatabase, FakeDbError, Procedure, ResultSet, Table, Value, View};

fn t(s: &str) -> Value {
    Value::Text(s.to_string())
}
fn i(v: i64) -> Value {
    Value::Int(v)
}
fn f(v: f64) -> Value {
    Value::Float(v)
}

/// Build the demo database.
pub fn demo_database() -> FakeDatabase {
    let mut db = FakeDatabase::new();

    db.add_table(Table {
        name: "Customers".into(),
        columns: vec![
            Column::new("CustomerID", "nchar(5)"),
            Column::new("CompanyName", "nvarchar(40)"),
            Column::new("ContactName", "nvarchar(30)"),
            Column::new("Country", "nvarchar(15)"),
        ],
        rows: vec![
            vec![
                t("ALFKI"),
                t("Alfreds Futterkiste"),
                t("Maria Anders"),
                t("Germany"),
            ],
            vec![
                t("ANATR"),
                t("Ana Trujillo Emparedados"),
                t("Ana Trujillo"),
                t("Mexico"),
            ],
            vec![
                t("ANTON"),
                t("Antonio Moreno Taquería"),
                t("Antonio Moreno"),
                t("Mexico"),
            ],
            vec![t("AROUT"), t("Around the Horn"), t("Thomas Hardy"), t("UK")],
            vec![
                t("BERGS"),
                t("Berglunds snabbköp"),
                t("Christina Berglund"),
                t("Sweden"),
            ],
            vec![
                t("BLAUS"),
                t("Blauer See Delikatessen"),
                t("Hanna Moos"),
                t("Germany"),
            ],
            vec![t("QUICK"), t("QUICK-Stop"), t("Horst Kloss"), t("Germany")],
        ],
    });

    db.add_table(Table {
        name: "Products".into(),
        columns: vec![
            Column::new("ProductID", "int"),
            Column::new("ProductName", "nvarchar(40)"),
            Column::new("CategoryName", "nvarchar(15)"),
            Column::new("UnitPrice", "money"),
            Column::new("UnitsInStock", "smallint"),
        ],
        rows: vec![
            vec![i(1), t("Chai"), t("Beverages"), f(18.0), i(39)],
            vec![i(2), t("Chang"), t("Beverages"), f(19.0), i(17)],
            vec![i(3), t("Aniseed Syrup"), t("Condiments"), f(10.0), i(13)],
            vec![
                i(24),
                t("Guaraná Fantástica"),
                t("Beverages"),
                f(4.5),
                i(20),
            ],
            vec![i(38), t("Côte de Blaye"), t("Beverages"), f(263.5), i(17)],
            vec![
                i(29),
                t("Thüringer Rostbratwurst"),
                t("Meat/Poultry"),
                f(123.79),
                i(0),
            ],
            vec![i(62), t("Tarte au sucre"), t("Confections"), f(49.3), i(17)],
            vec![
                i(60),
                t("Camembert Pierrot"),
                t("Dairy Products"),
                f(34.0),
                i(19),
            ],
        ],
    });

    db.add_table(Table {
        name: "Orders".into(),
        columns: vec![
            Column::new("OrderID", "int"),
            Column::new("CustomerID", "nchar(5)"),
            Column::new("OrderDate", "datetime"),
            Column::new("Freight", "money"),
        ],
        rows: vec![
            vec![i(10643), t("ALFKI"), t("1997-08-25"), f(29.46)],
            vec![i(10692), t("ALFKI"), t("1997-10-03"), f(61.02)],
            vec![i(10702), t("ALFKI"), t("1997-10-13"), f(23.94)],
            vec![i(10308), t("ANATR"), t("1996-09-18"), f(1.61)],
            vec![i(10365), t("ANTON"), t("1996-11-27"), f(22.0)],
            vec![i(10383), t("AROUT"), t("1996-12-16"), f(34.24)],
            vec![i(10278), t("BERGS"), t("1996-08-12"), f(92.69)],
            vec![i(10254), t("QUICK"), t("1996-07-11"), f(22.98)],
            vec![i(10255), t("QUICK"), t("1996-07-12"), f(148.33)],
        ],
    });

    db.add_table(Table {
        name: "Employees".into(),
        columns: vec![
            Column::new("EmployeeID", "int"),
            Column::new("LastName", "nvarchar(20)"),
            Column::new("FirstName", "nvarchar(10)"),
            Column::new("Title", "nvarchar(30)"),
        ],
        rows: vec![
            vec![i(1), t("Davolio"), t("Nancy"), t("Sales Representative")],
            vec![i(2), t("Fuller"), t("Andrew"), t("Vice President, Sales")],
            vec![i(3), t("Leverling"), t("Janet"), t("Sales Representative")],
            vec![i(5), t("Buchanan"), t("Steven"), t("Sales Manager")],
        ],
    });

    db.add_view(View {
        name: "ExpensiveProducts".into(),
        definition: "SELECT ProductName, CategoryName, UnitPrice FROM Products \
                     WHERE UnitPrice > 30 ORDER BY UnitPrice DESC"
            .into(),
        description: "Products priced above 30, most expensive first.".into(),
    });

    db.add_view(View {
        name: "GermanCustomers".into(),
        definition: "SELECT CustomerID, CompanyName, ContactName FROM Customers \
                     WHERE Country = 'Germany'"
            .into(),
        description: "Customers based in Germany.".into(),
    });

    db.add_procedure(Procedure {
        name: "TopProductsByPrice".into(),
        params: vec!["@count".into()],
        description: "The @count most expensive products.".into(),
        body: "CREATE PROCEDURE TopProductsByPrice @count int AS\n\
               SELECT TOP (@count) * FROM Products ORDER BY UnitPrice DESC"
            .into(),
    });

    db.add_procedure(Procedure {
        name: "CustomerOrders".into(),
        params: vec!["@customer_id".into()],
        description: "All orders placed by one customer.".into(),
        body: "CREATE PROCEDURE CustomerOrders @customer_id nchar(5) AS\n\
               SELECT * FROM Orders WHERE CustomerID = @customer_id"
            .into(),
    });

    db.add_procedure(Procedure {
        name: "CustomersByCountry".into(),
        params: vec![],
        description: "Customer counts per country.".into(),
        body: "CREATE PROCEDURE CustomersByCountry AS\n\
               SELECT Country, COUNT(*) AS Customers FROM Customers GROUP BY Country"
            .into(),
    });

    db
}

/// Execute one of the demo procedures by (case-insensitive) name.
pub fn run_demo_procedure(
    db: &FakeDatabase,
    name: &str,
    args: &[Value],
) -> Result<ResultSet, FakeDbError> {
    let lname = name.to_lowercase();
    match lname.as_str() {
        "topproductsbyprice" => {
            let count = match args.first() {
                Some(Value::Int(n)) if *n >= 0 => *n as usize,
                None => 5,
                other => {
                    return Err(FakeDbError(format!(
                        "TopProductsByPrice expects an integer @count, got {other:?}"
                    )));
                }
            };
            let out = db.execute(&format!(
                "SELECT * FROM Products ORDER BY UnitPrice DESC LIMIT {count}"
            ))?;
            Ok(out.result_sets.into_iter().next().unwrap())
        }
        "customerorders" => {
            let id = match args.first() {
                Some(Value::Text(id)) => id.clone(),
                other => {
                    return Err(FakeDbError(format!(
                        "CustomerOrders expects a text @customer_id, got {other:?}"
                    )));
                }
            };
            let out = db.execute(&format!("SELECT * FROM Orders WHERE CustomerID = '{id}'"))?;
            Ok(out.result_sets.into_iter().next().unwrap())
        }
        "customersbycountry" => {
            // GROUP BY is not in the query subset; compute directly.
            let customers = db
                .table("Customers")
                .ok_or_else(|| FakeDbError("Customers table missing".into()))?;
            let country_idx = customers
                .columns
                .iter()
                .position(|c| c.name == "Country")
                .ok_or_else(|| FakeDbError("Country column missing".into()))?;
            let mut counts: std::collections::BTreeMap<String, i64> = Default::default();
            for row in &customers.rows {
                if let Value::Text(country) = &row[country_idx] {
                    *counts.entry(country.clone()).or_default() += 1;
                }
            }
            Ok(ResultSet {
                columns: vec![
                    Column::new("Country", "nvarchar(15)"),
                    Column::new("Customers", "int"),
                ],
                rows: counts
                    .into_iter()
                    .map(|(country, n)| vec![Value::Text(country), Value::Int(n)])
                    .collect(),
            })
        }
        _ => Err(FakeDbError(format!(
            "Stored procedure '{name}' has no executable body in the demo database."
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn customers_by_country_groups() {
        let db = demo_database();
        let rs = run_demo_procedure(&db, "CustomersByCountry", &[]).unwrap();
        let germany = rs
            .rows
            .iter()
            .find(|r| r[0] == Value::Text("Germany".into()))
            .expect("Germany row");
        assert_eq!(germany[1], Value::Int(3));
    }

    #[test]
    fn top_products_defaults_to_five() {
        let db = demo_database();
        let rs = run_demo_procedure(&db, "TopProductsByPrice", &[]).unwrap();
        assert_eq!(rs.rows.len(), 5);
    }
}
