CREATE TABLE customers (
  id          integer PRIMARY KEY,
  email       text NOT NULL,
  name        text NOT NULL,
  country     text NOT NULL,
  created_at  timestamptz NOT NULL
);
CREATE INDEX customers_email_idx ON customers (email);

CREATE TABLE products (
  id        integer PRIMARY KEY,
  sku       text NOT NULL,
  category  text NOT NULL,
  price     numeric(10,2) NOT NULL
);

CREATE TABLE orders (
  id           bigint PRIMARY KEY,
  customer_id  integer NOT NULL REFERENCES customers (id),
  status       text NOT NULL,
  placed_at    timestamptz NOT NULL,
  total        numeric(12,2) NOT NULL
);
CREATE INDEX orders_placed_at_idx ON orders (placed_at);

CREATE TABLE order_items (
  id          bigint PRIMARY KEY,
  order_id    bigint NOT NULL REFERENCES orders (id),
  product_id  integer NOT NULL REFERENCES products (id),
  quantity    integer NOT NULL,
  unit_price  numeric(10,2) NOT NULL
);
CREATE INDEX order_items_order_id_idx ON order_items (order_id);
