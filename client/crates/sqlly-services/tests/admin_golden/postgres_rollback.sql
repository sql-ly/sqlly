-- golden rollback;
-- MANUAL REVIEW: recreate public.legacy from a backup; definition not captured;
CREATE OR REPLACE VIEW public.vorders AS SELECT o.id, o.customer_id FROM public.orders o;
ALTER TABLE "public.products" RENAME TO "product";