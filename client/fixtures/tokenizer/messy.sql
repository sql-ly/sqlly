-- Intentionally messy real-world SQL for tokenizer robustness fixtures.
-- Mixed casing, irregular whitespace, inline + block comments, embedded
-- string literals that look like statements, GO batch separators, quoted and
-- bracketed identifiers, and a deliberately malformed trailing fragment.

create   TABLE dbo.[Weird Name]  (
    Id int   IDENTITY(1,1)  NOT NULL,   -- surrogate key
    /* the next column has a default that contains the word CREATE */
    Note nvarchar(400) NULL DEFAULT N'CREATE TABLE should not confuse us',
    ParentId INT NULL,
    CONSTRAINT [PK_Weird] PRIMARY KEY (Id),
    CONSTRAINT FK_Weird_Parent FOREIGN KEY (ParentId) REFERENCES dbo.[Weird Name](Id),
    CONSTRAINT CK_Weird_Note CHECK (LEN(Note) > 0)
)
GO

-- A view with a tricky body and a string that mentions PROCEDURE.
CREATE VIEW [dbo].[v_messy] AS
    SELECT w.Id,
           w.Note AS [The Note],
           'this is not a CREATE PROCEDURE' AS Hint
    FROM dbo.[Weird Name] AS w
    WHERE w.Note <> N'';
GO

CREATE PROCEDURE dbo.usp_Messy
    @Search   nvarchar(100) = N'%',
    @Limit    int           = 100,
    @Output   bit           = 0 OUTPUT
AS
BEGIN
    SET NOCOUNT ON;
    -- result set 1
    SELECT TOP (@Limit) Id, Note FROM dbo.[Weird Name] WHERE Note LIKE @Search;
END
GO

create function dbo.fn_Messy(@x int) returns int
as
begin
    return @x * 2;
end
GO

-- Deliberately malformed trailing fragment to exercise diagnostics.
CREATE TABLE dbo.Broken (
    Id int
