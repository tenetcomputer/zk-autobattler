use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use tokio;

// DB
use mongodb::bson::doc;
use mongodb::bson::Document;
use mongodb::{Collection, Database};

// ZK VM
use risc0_zkvm::serde::{from_slice, to_vec};
use risc0_zkvm::Prover;

// Custom Modules
use methods::{TENET_ARENA_1_ID, TENET_ARENA_1_PATH};

use crate::models::games;

pub async fn get_all_games(State(db): State<Database>) -> impl IntoResponse {
    tracing::info!("get_all_games called");

    let games = db.collection::<Document>("game");
    // get all games that have state complete
    let mut cursor = games
        .find(
            doc! {
                "state": "complete"
            },
            None,
        )
        .await
        .unwrap();
    let mut games: Vec<games::Game> = Vec::new();

    // go through each document
    while cursor.advance().await.unwrap() {
        let game = bson::to_bson(&cursor.deserialize_current().unwrap()).unwrap();
        let mut game = bson::from_bson::<games::Game>(game).unwrap();
        game.id = None;
        game.creation1 = None;
        game.creation2 = None;
        game.lobby_id = String::from("");
        games.push(game);
    }

    let response = games::GetGamesOutput {
        games: games,
        error: String::from(""),
    };

    (StatusCode::OK, Json(response))
}

pub async fn join_game(
    // this argument tells axum to parse the request body
    State(db): State<Database>,
    Json(payload): Json<games::JoinGameInput>,
) -> impl IntoResponse {
    tracing::info!("join_game called");

    let mut response = games::JoinGameOutput {
        lobby_id: String::from(""),
        error: String::from(""),
    };
    let lobbies = db.collection::<Document>("lobby");

    let player_id: String = payload.player_id;
    let lobby_id: String = payload.lobby_id;
    if lobby_id.is_empty() {
        // check for existing open lobbies
        let open_lobby = lobbies
            .find_one(
                doc! {
                    "player2_id": null,
                    "player1_id": {
                        "$ne": player_id.clone()
                    }
                },
                None,
            )
            .await
            .unwrap();
        if !payload.create_new && open_lobby.is_some() {
            // join the lobby
            let lobby = open_lobby.unwrap();
            let lobby_id = lobby.get("_id").unwrap().as_object_id().unwrap();
            let update_result = lobbies
                .update_one(
                    doc! {
                        "_id": lobby_id,
                    },
                    doc! {
                        "$set": { "player2_id": player_id }
                    },
                    None,
                )
                .await
                .unwrap();

            response.lobby_id = lobby_id.to_string();
        } else {
            // if no open lobbies, create a new one
            let new_lobby = doc! {
                "lobby_id": null,
                "player1_id": player_id,
                "player2_id": null,
            };
            let insert_result = lobbies.insert_one(new_lobby.clone(), None).await.unwrap();
            let newlobby_id = insert_result.inserted_id.as_object_id().unwrap();

            let update_result = lobbies
                .update_one(
                    doc! {
                        "_id": newlobby_id,
                    },
                    doc! {
                        "$set": { "lobby_id": newlobby_id.to_string() }
                    },
                    None,
                )
                .await
                .unwrap();

            response.lobby_id = newlobby_id.to_string();
        }
    } else {
        // join this specific lobby, fail if already full
        let update_result = lobbies
            .update_one(
                doc! {
                    "lobby_id": lobby_id.clone(),
                    "player1_id": {
                        "$ne": player_id.clone()
                    },
                    "player2_id": null,
                },
                doc! {
                    "$set": { "player2_id": player_id }
                },
                None,
            )
            .await
            .unwrap();
        if update_result.modified_count == 1 {
            response.lobby_id = lobby_id;
        } else {
            // TODO: Separate is full vs does not exist vs already in it
            response.error = String::from("Lobby is full or does not exist");
            return (StatusCode::BAD_REQUEST, Json(response));
        }
    }

    return (StatusCode::OK, Json(response));
}

async fn commence_battle(game: &games::Game) -> risc0_zkvm::Receipt {
    // start the battle with both user inputs
    let arena_src = std::fs::read(TENET_ARENA_1_PATH)
    .expect("Method code should be present at the specified path; did you use the correct *_PATH constant?");

    let prover_opts = risc0_zkvm::ProverOpts::default().with_skip_seal(true);
    let mut prover = Prover::new_with_opts(&arena_src, TENET_ARENA_1_ID, prover_opts).expect(
        "Prover should be constructed from valid method source code and corresponding method ID",
    );

    // Next we send a & b to the guest
    prover.add_input_u32_slice(&to_vec(&game.player1_id).unwrap().as_slice());
    prover.add_input_u32_slice(&to_vec(&game.creation1.unwrap()).unwrap().as_slice());
    prover.add_input_u32_slice(&to_vec(&game.player2_id).unwrap().as_slice());
    prover.add_input_u32_slice(&to_vec(&game.creation2.unwrap()).unwrap().as_slice());

    tracing::info!("Starting proof");

    // Run prover & generate receipt
    let receipt = prover.run()
    .expect("Valid code should be provable if it doesn't overflow the cycle limit. See `embed_methods_with_options` for information on adjusting maximum cycle count.");

    tracing::info!("Proof done!");

    return receipt;
}

async fn commit_game_result(
    games_ref: Collection<Document>,
    game: &games::Game,
    receipt: &risc0_zkvm::Receipt,
) {
    // Verify receipt
    // HACK: Verification turned off, since seal is skipped for performance reasons
    // receipt
    //     .verify(&TENET_ARENA_1_ID)
    //     .expect("Receipt should be valid for the given method ID");

    // battle has finished update the game document
    // remove the user creations and add the battle result
    let vec = &receipt.journal;
    let game_result: tenet_core::GameResult = from_slice(vec).unwrap();


    // Sanity check
    assert!(*game.creation1_hash.as_ref().unwrap() == game_result.creation1_hash);
    assert!(*game.creation2_hash.as_ref().unwrap() == game_result.creation2_hash);

    if !game_result.error.is_empty() {
        let update_result = games_ref
        .update_one(
            doc! {
                "_id": game.id,
            },
            doc! {
                "$set": doc! {
                    "state": "error",
                    "error": game_result.error.clone()
                },
            },
            None,
        )
        .await
        .unwrap();
    } else {
        let mut new_game_doc = doc! {
            "winner_creation_hash": null,
            "winner_id": null,
            "result": game_result.result.clone(),
            "state": "complete"
        };

        if !game_result.winner_creation_hash.is_empty() {
            new_game_doc.insert(
                "winner_creation_hash",
                game_result.winner_creation_hash.clone(),
            );
            new_game_doc.insert("winner_id", game_result.winner_id.clone());
        }

        let update_result = games_ref
            .update_one(
                doc! {
                    "_id": game.id,
                },
                doc! {
                    "$set": new_game_doc,
                    "$unset": { "creation1": "", "creation2": "" }
                },
                None,
            )
            .await
            .unwrap();

    }

}

// TODO: Which hash function to use?
pub async fn play_game(
    // this argument tells axum to parse the request body
    State(db): State<Database>,
    Json(payload): Json<games::PlayGameInput>,
) -> impl IntoResponse {
    tracing::info!("play_game called");

    let lobby_id = payload.lobby_id;
    let mut response = games::PlayGameOutput {
        error: String::from(""),
    };

    // check if lobby exists
    let lobbies = db.collection::<Document>("lobby");
    let lobby = lobbies
        .find_one(
            doc! {
                "lobby_id": lobby_id.clone(),
            },
            None,
        )
        .await
        .unwrap();
    if lobby.is_none() {
        response.error = String::from("Lobby does not exist");
        return (StatusCode::BAD_REQUEST, Json(response));
    }

    // lobby.unwrap()
    let lobby = bson::to_bson(&lobby.unwrap()).unwrap();
    let lobby = bson::from_bson::<games::Lobby>(lobby).unwrap();
    // from_bson::<games::Lobby>(lobby);

    // check if player ids exist, otherwise return
    if lobby.player1_id.is_none() || lobby.player2_id.is_none() {
        response.error = String::from("Lobby is not full");
        return (StatusCode::BAD_REQUEST, Json(response));
    }

    let player1_id = lobby.player1_id.unwrap();
    let player2_id = lobby.player2_id.unwrap();
    let is_player_1 = player1_id == payload.player_id;

    if !is_player_1 && player2_id != payload.player_id {
        response.error = String::from("Player is not in this lobby");
        return (StatusCode::BAD_REQUEST, Json(response));
    }

    // check if game document exists
    let games = db.collection::<Document>("game");
    let game = games
        .find_one(
            doc! {
                "lobby_id": lobby_id.clone(),
            },
            None,
        )
        .await
        .unwrap();

    if game.is_none() {
        let arena_hash = TENET_ARENA_1_ID;
        let mut s = DefaultHasher::new();
        arena_hash.hash(&mut s);
        let arena_hash = s.finish().to_string();

        let creation_bson = bson::to_bson(&payload.creation).unwrap();

        let creation_hash = payload.creation;
        let mut s = DefaultHasher::new();
        creation_hash.hash(&mut s);
        let creation_hash = s.finish().to_string();

        let mut new_game = doc! {
            "lobby_id": lobby_id,
            "player1_id": player1_id,
            "player2_id": player2_id,
            "creation1": null,
            "creation1_hash": null,
            "creation2": null,
            "creation2_hash": null,
            "arena_hash": arena_hash,
            "winner_creation_hash": null,
            "winner_id": null,
            "state": null,
            "result": null,
            "error": null
        };

        if is_player_1 {
            new_game.insert("state", "player2Turn");
            new_game.insert("creation1", creation_bson);
            new_game.insert("creation1_hash", creation_hash);
        } else {
            new_game.insert("state", "player1Turn");
            new_game.insert("creation2", creation_bson);
            new_game.insert("creation2_hash", creation_hash);
        }

        // create it
        let insert_result = games.insert_one(new_game.clone(), None).await.unwrap();
    } else {
        // game exists, check if it's in the right state
        let game_doc = game.unwrap();
        let game_id = game_doc.get("_id").unwrap().as_object_id().unwrap();
        let game = bson::to_bson(&game_doc).unwrap();
        let game = bson::from_bson::<games::Game>(game).unwrap();

        if (game.state == "player1Turn" && !is_player_1)
            || (game.state == "player2Turn" && is_player_1)
        {
            response.error = String::from("It's not your turn");
            return (StatusCode::BAD_REQUEST, Json(response));
        }

        if game.state == "playing" {
            response.error = String::from("Game is in progress");
            return (StatusCode::BAD_REQUEST, Json(response));
        } else if game.state == "complete" {
            response.error = String::from("Game is finished");
            return (StatusCode::BAD_REQUEST, Json(response));
        }

        let creation_bson = bson::to_bson(&payload.creation).unwrap();
        let creation_hash = payload.creation;
        let mut s = DefaultHasher::new();
        creation_hash.hash(&mut s);
        let creation_hash = s.finish().to_string();

        let mut new_game_doc = None;

        // Check if creation exists
        if game.state == "player1Turn" {
            if game.creation1_hash.is_some()
                && creation_hash == *game.creation1_hash.as_ref().unwrap()
            {
                // COMMENCE AUTO BATTLE
                new_game_doc = Some(doc! {
                    "$set": {
                        "state": "playing",
                    }
                });
                let games_ref = games.clone();
                let game_thread = game.clone();
                tokio::task::spawn(async move {
                    // TODO: Catch error in proof of battle
                    let receipt = commence_battle(&game_thread).await;
                    commit_game_result(games_ref, &game_thread, &receipt).await;
                });
                // // println!("Receipt: {:?}", committed_state);
            } else {
                new_game_doc = Some(doc! {
                    "$set": {
                        "creation1": creation_bson,
                        "creation1_hash": creation_hash,
                        "state": "player2Turn",
                    }
                });
            }
        } else if game.state == "player2Turn" {
            if game.creation2_hash.is_some()
                && creation_hash == *game.creation2_hash.as_ref().unwrap()
            {
                // COMMENCE AUTO BATTLE
                new_game_doc = Some(doc! {
                    "$set": {
                        "state": "playing",
                    }
                });
                let games_ref = games.clone();
                let game_thread = game.clone();
                tokio::task::spawn(async move {
                    // TODO: Catch error in proof of battle
                    let receipt = commence_battle(&game_thread).await;
                    commit_game_result(games_ref, &game_thread, &receipt).await;
                });
            } else {
                // update game state
                new_game_doc = Some(doc! {
                    "$set": {
                        "creation2": creation_bson,
                        "creation2_hash": creation_hash,
                        "state": "player1Turn",
                    }
                });
            }
        }

        // update game state
        let update_result = games
            .update_one(
                doc! {
                    "_id": game_id,
                },
                new_game_doc.unwrap(),
                None,
            )
            .await
            .unwrap();
        // Check if creation changed
    }

    return (StatusCode::OK, Json(response));
}

pub async fn play_npc_game(
    // this argument tells axum to parse the request body
    State(db): State<Database>,
    Json(payload): Json<games::PlayNPCGameInput>,
) -> impl IntoResponse {
    tracing::info!("play_npc_game called");

    let mut response = games::PlayNPCGameOutput {
        error: String::from(""),
    };

    // Check if player has battled this NPC before by checking if game exists with player creation and NCP creation
    let games = db.collection("game");

    let player_creation_hah = payload.creation;
    let mut s = DefaultHasher::new();
    player_creation_hah.hash(&mut s);
    let player_creation_hash = s.finish().to_string();

    let npc_creation_hash = payload.npc_creation;
    let mut s = DefaultHasher::new();
    npc_creation_hash.hash(&mut s);
    let npc_creation_hash = s.finish().to_string();

    let game = games
        .find_one(
            doc! {
                "player1_id": payload.player_id.clone(),
                "creation1_hash": player_creation_hash.clone(),
                "creation2_hash": npc_creation_hash.clone(),
            },
            None,
        )
        .await
        .unwrap();

    if game.is_some() {
        // game played, return error
        response.error = String::from("You have already played this NPC with this deck");
        return (StatusCode::BAD_REQUEST, Json(response));
    }

    let lobbies = db.collection("lobby");

    // Create new lobby with player and NPC
    let new_lobby = doc! {
        "lobby_id": null,
        "player1_id": payload.player_id.clone(),
        "player2_id": payload.npc_id.clone(),
    };
    let insert_result = lobbies.insert_one(new_lobby.clone(), None).await.unwrap();
    let newlobby_id = insert_result.inserted_id.as_object_id().unwrap();

    let update_result = lobbies
        .update_one(
            doc! {
                "_id": newlobby_id,
            },
            doc! {
                "$set": { "lobby_id": newlobby_id.to_string() }
            },
            None,
        )
        .await
        .unwrap();

    // create new game

    let arena_hash = TENET_ARENA_1_ID;
    let mut s = DefaultHasher::new();
    arena_hash.hash(&mut s);
    let arena_hash = s.finish().to_string();

    let creation1_bson = bson::to_bson(&payload.creation).unwrap();
    let creation2_bson = bson::to_bson(&payload.npc_creation).unwrap();

    let mut new_game = doc! {
        "lobby_id": newlobby_id.to_string(),
        "player1_id": payload.player_id.clone(),
        "player2_id": payload.npc_id.clone(),
        "creation1": creation1_bson,
        "creation1_hash": player_creation_hash.clone(),
        "creation2": creation2_bson,
        "creation2_hash": npc_creation_hash.clone(),
        "arena_hash": arena_hash,
        "winner_creation_hash": null,
        "winner_id": null,
        "state": "playing",
        "result": null
    };

    let insert_result = games.insert_one(new_game.clone(), None).await.unwrap();

    // get inserted game
    let game_id = insert_result.inserted_id.as_object_id().unwrap();
    let game_doc = games
        .find_one(
            doc! {
                "_id": game_id,
            },
            None,
        )
        .await
        .unwrap()
        .unwrap();

    let game_id = game_doc.get("_id").unwrap().as_object_id().unwrap();
    let game = bson::to_bson(&game_doc).unwrap();
    let game = bson::from_bson::<games::Game>(game).unwrap();

    let games_ref = games.clone();
    let game_thread = game.clone();
    tokio::task::spawn(async move {
        // TODO: Catch error in proof of battle
        let receipt = commence_battle(&game_thread).await;
        commit_game_result(games_ref, &game_thread, &receipt).await;
    });

    return (StatusCode::OK, Json(response));
}

pub async fn commit_outcome(
    // this argument tells axum to parse the request body
    State(db): State<Database>,
    Json(payload): Json<games::CommitOutcomeInput>,
) -> impl IntoResponse {
    tracing::info!("commit_outcome called");

    let out = "Done";

    (StatusCode::OK, out)
}
