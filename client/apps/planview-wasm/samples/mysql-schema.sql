CREATE TABLE customers (
  id int PRIMARY KEY,
  name varchar(60) NOT NULL,
  email varchar(80) NOT NULL,
  country char(2) NOT NULL,
  created_at datetime NOT NULL,
  KEY customers_email_idx (email)
);

CREATE TABLE products (
  id int PRIMARY KEY,
  sku varchar(20) NOT NULL,
  name varchar(60) NOT NULL,
  category varchar(20) NOT NULL,
  price decimal(10,2) NOT NULL
);

CREATE TABLE orders (
  id int PRIMARY KEY,
  customer_id int NOT NULL,
  status varchar(12) NOT NULL,
  placed_at datetime NOT NULL,
  total decimal(12,2) NOT NULL,
  KEY orders_customer_idx (customer_id),
  KEY orders_placed_idx (placed_at)
);

CREATE TABLE order_items (
  id bigint PRIMARY KEY,
  order_id int NOT NULL,
  product_id int NOT NULL,
  quantity int NOT NULL,
  unit_price decimal(10,2) NOT NULL,
  KEY order_items_order_idx (order_id)
);
