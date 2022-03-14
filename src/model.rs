pub mod sticker {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "sticker")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,

        #[sea_orm(unique)]
        pub file_id: String,

        pub set_name: String,

        pub popularity: i64,
    }

    #[derive(Debug, DeriveRelation, EnumIter)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod tagged_sticker {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "tagged_sticker")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,

        #[sea_orm(column_type = "Text")]
        pub tag: String,
        pub sticker_id: i32,
        pub tagger_id: i32,

        pub ts: DateTimeUtc,
    }

    #[derive(Debug, DeriveRelation, EnumIter)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod user {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "allowed_user")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,

        #[sea_orm(unique)]
        pub user_id: i64,

        #[sea_orm(column_type = "Text")]
        pub username: String,
        pub allowed: bool,
    }

    #[derive(Debug, DeriveRelation, EnumIter)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}
