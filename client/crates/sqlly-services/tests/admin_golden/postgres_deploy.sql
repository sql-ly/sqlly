-- golden deploy;
ALTER TABLE "public.product" RENAME TO "products";;
CREATE OR REPLACE VIEW public.vorders AS SELECT o.id FROM public.orders o;
DROP TABLE "public"."legacy";