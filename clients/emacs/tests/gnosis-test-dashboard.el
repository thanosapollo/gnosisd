;;; gnosis-test-dashboard.el --- Dashboard projection tests  -*- lexical-binding: t; -*-

;;; Code:

(require 'cl-lib)
(require 'ert)
(require 'gnosis)

(defun gnosis-test-dashboard--projection (&optional active-session)
  "Return a server dashboard projection with ACTIVE-SESSION."
  `(:generated_at_us 1 :review_day 42
    :today (:review_count 8 :new_count 2 :success_count 6
            :failure_count 2 :override_count 1)
    :window (:from_day 13 :through_day 42 :days 30 :active_days 4
             :totals (:review_count 20 :new_count 3 :success_count 15
                      :failure_count 5 :override_count 2)
             :average_per_calendar_day 0.67 :average_per_active_day 5.0
             :success_rate 0.75)
    :queue (:new_count 3 :due_count 4 :overdue_count 1 :suspended_count 2)
    :content (:active_themata 12 :archived_themata 3
              :active_documents 2 :active_nodes 7)
    :active_session ,(or active-session gnosis-core--json-null)))

(defun gnosis-test-dashboard--thema (&rest overrides)
  "Return a valid thema catalog record with OVERRIDES."
  (let ((record
         (copy-tree
          `(:thema_id "thema-a" :thema_revision 3 :state_revision 2
            :kind "basic" :keimenon "Question"
            :hypothesis [,gnosis-core--json-null]
            :answer ["Answer"] :tags ["tag"]
            :archived ,gnosis-core--json-false
            :suspended ,gnosis-core--json-false)
          t)))
    (while overrides
      (setq record (plist-put record (pop overrides) (pop overrides))))
    record))

(ert-deftest gnosis-dashboard-renders-only-server-projected-values ()
  (when-let* ((buffer (get-buffer gnosis-dashboard-buffer-name)))
    (kill-buffer buffer))
  (unwind-protect
      (cl-letf (((symbol-function 'gnosis-core-dashboard)
                 #'gnosis-test-dashboard--projection)
                ((symbol-function 'keymap-popup) #'ignore))
        (save-window-excursion (gnosis-dashboard))
        (with-current-buffer gnosis-dashboard-buffer-name
          (should (derived-mode-p 'gnosis-dashboard-mode))
          (should (eq 'gnosis-face-dashboard-header
                      (get-text-property (point-min) 'face)))
          (dolist (text '("Connection: Ready"
                          "Reviewed today: 8 (New: 2)"
                          "Due themata: 4 (Overdue: 1)"
                          "Content: 12 themata, 3 archived, 2 documents, 7 nodes"
                          "Last 30 days: 20 reviews, success rate 0.75"))
            (should (string-match-p (regexp-quote text) (buffer-string))))
          (should-not
           (string-match-p "Review day\\|Queue:\\|Active session"
                           (buffer-string)))))
    (when-let* ((buffer (get-buffer gnosis-dashboard-buffer-name)))
      (kill-buffer buffer))))

(ert-deftest gnosis-entry-opens-dashboard ()
  (let (called)
    (cl-letf (((symbol-function 'gnosis-dashboard)
               (lambda () (interactive) (setq called t)))
              ((symbol-function 'gnosis-review)
               (lambda () (interactive)
                 (ert-fail "Gnosis started review instead of the dashboard"))))
      (call-interactively #'gnosis))
    (should called)))

(ert-deftest gnosis-dashboard-shows-only-active-session-warning ()
  (let ((text
         (gnosis--dashboard-text
          (gnosis-test-dashboard--projection
           '(:session_id "private-session-id" :owner_device_id "owner"
             :mode "standard" :source (:kind "due") :target_count 10
             :status "active" :current_position 3 :queue_length 7
             :started_at_us 1)))))
    (should (string-match-p "Active session in progress" text))
    (should (eq (get-text-property
                 (string-match "Active session in progress" text) 'face text)
                'warning))
    (should-not
     (string-match-p
      "private-session-id\\|owner\\|standard\\|position\\|queue length"
      text))))

(ert-deftest gnosis-dashboard-renders-missing-success-rate ()
  (let* ((state (copy-tree (gnosis-test-dashboard--projection)))
         (window (plist-get state :window)))
    (setf (plist-get window :success_rate) gnosis-core--json-null)
    (with-temp-buffer
      (gnosis-dashboard-mode)
      (gnosis--render-dashboard state)
      (goto-char (point-min))
      (should (search-forward "success rate unavailable" nil t))
      (should (eq 'shadow (get-text-property (1- (point)) 'face))))))

(ert-deftest gnosis-dashboard-refresh-preserves-buffer-on-failure ()
  (with-temp-buffer
    (gnosis-dashboard-mode)
    (let ((inhibit-read-only t)) (insert "existing projection"))
    (cl-letf (((symbol-function 'gnosis-core-dashboard)
               (lambda (&rest _) (signal 'gnosis-core-unavailable '("down")))))
      (let ((condition (should-error (gnosis-dashboard-refresh)
                                     :type 'user-error)))
        (should
         (equal (cadr condition)
                "gnosisd unavailable. Check service and gnosis-core-base-url.")))
      (should (equal "existing projection" (buffer-string))))))

(ert-deftest gnosis-dashboard-keymap-is-direct-and-discoverable ()
  (should (eq (keymap-lookup gnosis-dashboard-mode-map "g")
              #'gnosis-dashboard-refresh))
  (should (eq (keymap-lookup gnosis-dashboard-mode-map "r")
              'gnosis-dashboard-review))
  (should (eq (keymap-lookup gnosis-dashboard-mode-map "n")
              'gnosis-nodes-find))
  (with-temp-buffer
    (setq gnosis-dashboard--projection
          (list :active_session '(:status "active")))
    (should (eq (keymap-lookup gnosis-dashboard-mode-map "R")
                'gnosis-review-resume)))
  (should (eq (keymap-lookup gnosis-dashboard-mode-map "t")
              'gnosis-dashboard-themata))
  (should (eq (keymap-lookup gnosis-dashboard-mode-map "T")
              'gnosis-review-topic))
  (should (eq (keymap-lookup gnosis-dashboard-mode-map "q") #'quit-window))
  (should (eq (keymap-lookup gnosis-dashboard-mode-map "?")
              'gnosis-dashboard-mode-map-popup)))

(ert-deftest gnosis-dashboard-popup-describes-start-resume-exactly ()
  (let* ((rows (keymap-popup--meta gnosis-dashboard-mode-map 'descriptions))
         (review-group (car (car rows)))
         (entry (seq-find
                 (lambda (item) (equal (plist-get item :key) "r"))
                 (plist-get review-group :entries))))
    (should (eq (plist-get entry :description)
                #'gnosis-dashboard--review-description))
    (with-temp-buffer
      (should (equal "Start review" (gnosis-dashboard--review-description)))
      (setq gnosis-dashboard--projection
            (list :active_session '(:status "active")))
      (should (equal "Resume review" (gnosis-dashboard--review-description))))
    (dolist (row rows)
      (dolist (group row)
        (should (<= (length (plist-get group :entries)) 4))))))

(ert-deftest gnosis-dashboard-renders-minimal-summary-with-state-faces ()
  (with-temp-buffer
    (gnosis-dashboard-mode)
    (gnosis--render-dashboard (gnosis-test-dashboard--projection))
    (dolist (text '("Connection" "Reviewed today" "Due themata"
                    "Content" "Last 30 days"))
      (should (string-search text (buffer-string))))
    (dolist (text '("Review day" "Queue:" "Active session"
                    "Review due" "Refresh" "Actions" "Quit"))
      (should-not (string-search text (buffer-string))))
    (goto-char (point-min))
    (search-forward "Connection:")
    (should (eq (get-text-property (1- (point)) 'face) 'shadow))
    (search-forward "Ready")
    (should (eq (get-text-property (1- (point)) 'face) 'success))
    (search-forward "6 successful")
    (should (eq (get-text-property (1- (point)) 'face) 'success))
    (search-forward "2 failed")
    (should (eq (get-text-property (1- (point)) 'face) 'error))
    (search-forward "4")
    (should (eq (get-text-property (1- (point)) 'face) 'warning))
    (search-forward "3 archived")
    (should (eq (get-text-property (1- (point)) 'face) 'shadow))))

(ert-deftest gnosis-dashboard-renders-separator-after-heading ()
  (with-temp-buffer
    (gnosis-dashboard-mode)
    (gnosis--render-dashboard (gnosis-test-dashboard--projection))
    (goto-char (point-min))
    (search-forward "Gnosis Dashboard")
    (forward-line 1)
    (should (equal (get-text-property (point) 'display)
                   '(space :width text)))
    (should (eq (get-text-property (point) 'face)
                'gnosis-face-separator))))

(ert-deftest gnosis-dashboard-mode-disables-line-numbers ()
  (with-temp-buffer
    (setq display-line-numbers 'relative)
    (gnosis-dashboard-mode)
    (should-not display-line-numbers)))

(ert-deftest gnosis-dashboard-reopening-preserves-local-state ()
  (let ((buffer (get-buffer-create gnosis-dashboard-buffer-name)))
    (unwind-protect
        (cl-letf (((symbol-function 'gnosis-core-dashboard)
                   #'gnosis-test-dashboard--projection)
                  ((symbol-function 'pop-to-buffer-same-window)
                   (lambda (target &rest _) (set-buffer target)))
                  ((symbol-function 'keymap-popup) #'ignore))
          (gnosis-dashboard)
          (with-current-buffer buffer
            (setq-local gnosis-center-content nil))
          (gnosis-dashboard)
          (with-current-buffer buffer
            (should-not gnosis-center-content)
            (goto-char (point-min))
            (should (looking-at "Gnosis Dashboard"))))
      (when (buffer-live-p buffer) (kill-buffer buffer)))))

(ert-deftest gnosis-dashboard-opens-command-popup ()
  (let ((buffer (get-buffer-create gnosis-dashboard-buffer-name))
        popup)
    (unwind-protect
        (cl-letf (((symbol-function 'gnosis-core-dashboard)
                   #'gnosis-test-dashboard--projection)
                  ((symbol-function 'pop-to-buffer-same-window)
                   (lambda (target &rest _) (set-buffer target)))
                  ((symbol-function 'keymap-popup)
                   (lambda (map) (setq popup map))))
          (gnosis-dashboard)
          (should (eq popup gnosis-dashboard-mode-map)))
      (when (buffer-live-p buffer) (kill-buffer buffer)))))

(ert-deftest gnosis-dashboard-faces-window-label-and-dynamic-day-value ()
  (dolist (days '(30 7))
    (let* ((state (copy-tree (gnosis-test-dashboard--projection)))
           (window (plist-get state :window)))
      (setf (plist-get window :days) days)
      (with-temp-buffer
        (gnosis-dashboard-mode)
        (gnosis--render-dashboard state)
        (goto-char (point-min))
        (search-forward (format "Last %d days:" days))
        (let ((end (point))
              (start (match-beginning 0))
              (day-start (+ (match-beginning 0) 5)))
          (should (eq (get-text-property start 'face) 'shadow))
          (should (eq (get-text-property day-start 'face)
                      'font-lock-type-face))
          (should (eq (get-text-property (+ day-start
                                            (length (number-to-string days)))
                                         'face)
                      'shadow))
          (should (equal (buffer-substring-no-properties start end)
                         (format "Last %d days:" days))))))))

(ert-deftest gnosis-dashboard-initial-open-starts-at-top ()
  (let ((buffer (get-buffer-create gnosis-dashboard-buffer-name)))
    (unwind-protect
        (save-window-excursion
          (with-current-buffer buffer
            (let ((inhibit-read-only t))
              (erase-buffer)
              (insert (make-string 200 ?x)))
            (goto-char 50))
          (set-window-buffer (selected-window) buffer)
          (set-window-start (selected-window) 25)
          (cl-letf (((symbol-function 'gnosis-core-dashboard)
                     #'gnosis-test-dashboard--projection)
                    ((symbol-function 'keymap-popup) #'ignore))
            (gnosis-dashboard))
          (should (= (with-current-buffer buffer (point)) (point-min)))
          (should (= (window-start (selected-window)) (point-min))))
      (when (buffer-live-p buffer) (kill-buffer buffer)))))

(ert-deftest gnosis-dashboard-refresh-preserves-numeric-point ()
  (with-temp-buffer
    (gnosis-dashboard-mode)
    (gnosis--render-dashboard (gnosis-test-dashboard--projection))
    (goto-char 25)
    (cl-letf (((symbol-function 'gnosis-core-dashboard)
               #'gnosis-test-dashboard--projection))
      (gnosis-dashboard-refresh))
    (should (= (point) 25))))

(ert-deftest gnosis-dashboard-refresh-restores-window-start ()
  (with-temp-buffer
    (gnosis-dashboard-mode)
    (let (restored)
      (cl-letf (((symbol-function 'selected-window) (lambda () 'window))
                ((symbol-function 'window-buffer)
                 (lambda (_window) (current-buffer)))
                ((symbol-function 'window-start) (lambda (&rest _) 11))
                ((symbol-function 'set-window-start)
                 (lambda (&rest arguments) (setq restored arguments)))
                ((symbol-function 'window-body-width) (lambda (&rest _) 80))
                ((symbol-function 'gnosis-core-dashboard)
                 #'gnosis-test-dashboard--projection))
        (gnosis-dashboard-refresh))
      (should (equal restored '(window 11 t))))))

(ert-deftest gnosis-dashboard-initial-failure-opens-safe-corrective-state ()
  (let ((buffer (get-buffer-create gnosis-dashboard-buffer-name)))
    (unwind-protect
        (cl-letf (((symbol-function 'gnosis-core-dashboard)
                   (lambda (&rest _)
                     (signal 'gnosis-core-authentication-error
                             '("secret server detail"))))
                  ((symbol-function 'pop-to-buffer-same-window)
                   (lambda (target &rest _) (set-buffer target)))
                  ((symbol-function 'keymap-popup) #'ignore))
          (gnosis-dashboard)
          (with-current-buffer buffer
            (should (derived-mode-p 'gnosis-dashboard-mode))
            (should-not gnosis-dashboard--projection)
            (should
             (string-search
              "Gnosis authentication failed. Check auth-source token for gnosisd."
              (buffer-string)))
            (should-not (string-search "secret server detail" (buffer-string)))
            (goto-char (point-min))
            (search-forward "authentication failed")
            (should (eq (get-text-property (1- (point)) 'face) 'error))))
      (when (buffer-live-p buffer) (kill-buffer buffer)))))

(ert-deftest gnosis-dashboard-primary-actions-dispatch-existing-journeys ()
  (let (review-arguments resumed)
    (cl-letf (((symbol-function 'gnosis-review)
               (lambda (&rest arguments) (setq review-arguments arguments)))
              ((symbol-function 'gnosis-review-resume)
               (lambda () (interactive) (setq resumed t))))
      (gnosis-dashboard-review)
      (with-temp-buffer
        (setq gnosis-dashboard--projection
              (list :active_session '(:status "active")))
        (call-interactively (keymap-lookup gnosis-dashboard-mode-map "R"))))
    (should (equal review-arguments '(nil)))
    (should resumed)))

(ert-deftest gnosis-themata-fetches-every-generation-bound-page ()
  (let ((first (gnosis-test-dashboard--thema))
        (second (gnosis-test-dashboard--thema :thema_id "thema-b"))
        calls)
    (cl-letf (((symbol-function 'gnosis-core-thema-catalog)
               (lambda (_query after limit)
                 (push (list after limit) calls)
                 (if after
                     `(:generation "feed-3" :items [,second]
                       :next ,gnosis-core--json-null)
                   `(:generation "feed-3" :items [,first] :next "thema-a")))))
      (let ((catalog (gnosis-themata--fetch)))
        (should (equal "feed-3" (plist-get catalog :generation)))
        (should
         (equal '("thema-a" "thema-b")
                (mapcar (lambda (record) (plist-get record :thema_id))
                        (append (plist-get catalog :items) nil)))))
      (should (equal '((nil 100) ("thema-a" 100)) (nreverse calls))))))

(ert-deftest gnosis-themata-fetch-rejects-generation-and-cursor-races ()
  (let ((calls 0))
    (cl-letf (((symbol-function 'gnosis-core-thema-catalog)
               (lambda (&rest _)
                 (cl-incf calls)
                 (if (= calls 1)
                     `(:generation "feed-3"
                       :items [,(gnosis-test-dashboard--thema)]
                       :next "thema-a")
                   `(:generation "feed-4" :items []
                     :next ,gnosis-core--json-null)))))
      (should-error (gnosis-themata--fetch) :type 'gnosis-core-conflict)))
  (cl-letf (((symbol-function 'gnosis-core-thema-catalog)
             (lambda (&rest _)
               '(:generation "feed-3" :items [] :next "thema-a"))))
    (should-error (gnosis-themata--fetch)
                  :type 'gnosis-core-protocol-error)))

(ert-deftest gnosis-themata-buffer-displays-archive-and-suspension-state ()
  (with-temp-buffer
    (gnosis-dashboard-themata-mode)
    (gnosis-themata--install
     `(:generation "feed-3"
       :items [,(gnosis-test-dashboard--thema
                 :archived t :suspended t)]))
    (should (derived-mode-p 'gnosis-dashboard-themata-mode))
    (should (string-search "Question" (buffer-string)))
    (should (string-search "Answer" (buffer-string)))
    (goto-char (point-min))
    (should (= 2 (how-many "\\_<Yes\\_>" (point-min) (point-max))))))

(ert-deftest gnosis-themata-suspension-mutates-only-after-authority ()
  (with-temp-buffer
    (gnosis-dashboard-themata-mode)
    (gnosis-themata--install
     `(:generation "feed-3"
       :items [,(gnosis-test-dashboard--thema)]))
    (gnosis-themata--goto-id "thema-a")
    (cl-letf (((symbol-function 'y-or-n-p) (lambda (&rest _) t))
              ((symbol-function 'gnosis-core-thema-set-suspended)
               (lambda (thema-id revision desired &optional _operation-id)
                 (should (equal thema-id "thema-a"))
                 (should (= revision 2))
                 (should desired)
                 '(:thema_id "thema-a"
                   :state (:state_revision 3 :suspended t)))))
      (gnosis-themata-toggle-suspension))
    (let ((record (gethash "thema-a" gnosis-themata--records)))
      (should (= 3 (plist-get record :state_revision)))
      (should (eq t (plist-get record :suspended))))
    (should (string-search "Yes" (buffer-string)))
    (cl-letf (((symbol-function 'y-or-n-p) (lambda (&rest _) t))
              ((symbol-function 'gnosis-core-thema-set-suspended)
               (lambda (_thema-id revision desired &optional _operation-id)
                 (should (= revision 3))
                 (should-not desired)
                 `(:thema_id "thema-a"
                   :state (:state_revision 4
                           :suspended ,gnosis-core--json-false)))))
      (gnosis-themata-toggle-suspension))
    (should (string-search "No" (buffer-string)))
    (should
     (eq gnosis-core--json-false
         (plist-get (gethash "thema-a" gnosis-themata--records)
                    :suspended)))))

(ert-deftest gnosis-themata-filters-preserve-selected-authoritative-row ()
  (let* ((alpha (gnosis-test-dashboard--thema
                 :thema_id "alpha" :keimenon "Alpha question"))
         (beta (gnosis-test-dashboard--thema
                :thema_id "beta" :keimenon "Beta question" :archived t))
         (catalog `(:generation "g" :items [,alpha ,beta])))
    (with-temp-buffer
      (gnosis-dashboard-themata-mode)
      (gnosis-themata--install catalog)
      (gnosis-themata--goto-id "alpha")
      (gnosis-themata-filter-search "Alpha")
      (should (equal "alpha" (tabulated-list-get-id)))
      (should (= 1 (length tabulated-list-entries)))
      (gnosis-themata-filter-review-state :all)
      (should (equal "alpha" (tabulated-list-get-id))))))

(ert-deftest gnosis-themata-archive-and-tag-actions-use-authority ()
  (let* ((record (gnosis-test-dashboard--thema
                  :thema_id "thema-a" :thema_revision 3 :tags ["tag"]))
         archived)
    (with-temp-buffer
      (gnosis-dashboard-themata-mode)
      (setq-local gnosis-themata--records (make-hash-table :test #'equal))
      (puthash "thema-a" record gnosis-themata--records)
      (setq-local tabulated-list-entries (list (gnosis-themata--entry record)))
      (tabulated-list-print t)
      (goto-char (point-min))
      (cl-letf (((symbol-function 'y-or-n-p) (lambda (&rest _) t))
                ((symbol-function 'gnosis-core-thema-set-archived)
                 (lambda (id revision desired)
                   (setq archived (list id revision desired))
                   (plist-put (copy-tree record) :archived t)))
                )
        (gnosis-themata-toggle-archive)
        (gnosis-themata-find-tag "tag"))
      (should (equal "tag" gnosis-themata--tag-filter)))
    (should (equal '("thema-a" 3 t) archived))))

(ert-deftest gnosis-themata-suspend-tag-mutates-visible-page-in-order ()
  (let ((first (gnosis-test-dashboard--thema
                :thema_id "first" :tags ["tag"] :state_revision 1))
        (second (gnosis-test-dashboard--thema
                 :thema_id "second" :tags ["tag"] :state_revision 2))
        calls)
    (with-temp-buffer
      (gnosis-dashboard-themata-mode)
      (gnosis-themata--install `(:generation "g" :items [,first ,second]))
      (gnosis-themata-find-tag "tag")
      (cl-letf (((symbol-function 'yes-or-no-p) (lambda (&rest _) t))
                ((symbol-function 'gnosis-core-thema-set-suspended)
                 (lambda (id revision desired)
                   (push (list id revision desired) calls)
                   `(:thema_id ,id
                     :state (:state_revision ,(1+ revision) :suspended t)))))
        (gnosis-themata-suspend-tag))
      (should (equal '(("first" 1 t) ("second" 2 t)) (nreverse calls)))
      (should (eq t (plist-get (gethash "first" gnosis-themata--records)
                               :suspended)))
      (should (eq t (plist-get (gethash "second" gnosis-themata--records)
                               :suspended))))))

(ert-deftest gnosis-themata-show-new-filters-current-bounded-page ()
  (let ((first (gnosis-test-dashboard--thema :thema_id "first"))
        (second (gnosis-test-dashboard--thema :thema_id "second")))
    (with-temp-buffer
      (gnosis-dashboard-themata-mode)
      (gnosis-themata--install `(:generation "g" :items [,first ,second]))
      (cl-letf (((symbol-function 'gnosis-core-thema-review-state)
                 (lambda (id)
                   `(:thema_id ,id
                     :state (:state_revision 1 :review_count
                             ,(if (equal id "first") 0 2)
                             :suspended ,gnosis-core--json-false)))))
        (gnosis-themata-show-new))
      (should (= 1 (length tabulated-list-entries)))
      (should (equal "first" (caar tabulated-list-entries))))))

(ert-deftest gnosis-themata-next-page-rejects-generation-race-without-advancing ()
  (let ((record (gnosis-test-dashboard--thema :thema_id "first")))
    (with-temp-buffer
      (gnosis-dashboard-themata-mode)
      (gnosis-themata--install
       `(:generation "stable" :items [,record] :next "cursor"))
      (cl-letf (((symbol-function 'gnosis-core-thema-catalog)
                 (lambda (&rest _)
                   `(:generation "changed" :items []
                     :next ,gnosis-core--json-null))))
        (should-error (gnosis-themata-next-page) :type 'gnosis-core-conflict))
      (should-not gnosis-themata--cursor)
      (should-not gnosis-themata--history)
      (should (equal "first" (tabulated-list-get-id))))))

(ert-deftest gnosis-themata-mutation-failures-preserve-visible-row ()
  (dolist (error '(gnosis-core-authorization-error
                   gnosis-core-conflict
                   gnosis-core-validation-error
                   gnosis-core-mutation-uncertain))
    (with-temp-buffer
      (gnosis-dashboard-themata-mode)
      (gnosis-themata--install
       `(:generation "feed-3"
         :items [,(gnosis-test-dashboard--thema)]))
      (gnosis-themata--goto-id "thema-a")
      (let ((before (buffer-string)))
        (cl-letf (((symbol-function 'y-or-n-p) (lambda (&rest _) t))
                  ((symbol-function 'gnosis-core-thema-set-suspended)
                   (lambda (&rest _) (signal error '("failure")))))
          (should-error (gnosis-themata-toggle-suspension) :type error))
        (should (equal before (buffer-string)))
        (should
         (eq gnosis-core--json-false
             (plist-get (gethash "thema-a" gnosis-themata--records)
                        :suspended)))))))

(ert-deftest gnosis-themata-refresh-preserves-buffer-on-failure ()
  (with-temp-buffer
    (gnosis-dashboard-themata-mode)
    (gnosis-themata--install
     `(:generation "feed-3"
       :items [,(gnosis-test-dashboard--thema)]))
    (let ((before (buffer-string))
          (generation gnosis-themata--generation))
      (cl-letf (((symbol-function 'gnosis-core-thema-catalog)
                 (lambda (&rest _)
                   (signal 'gnosis-core-unavailable '("offline")))))
        (should-error (gnosis-themata-refresh)
                      :type 'gnosis-core-unavailable))
      (should (equal before (buffer-string)))
      (should (equal generation gnosis-themata--generation)))))

(ert-deftest gnosis-themata-edit-opens-shared-editor-and-updates-one-row ()
  (with-temp-buffer
    (gnosis-dashboard-themata-mode)
    (gnosis-themata--install
     `(:generation "feed-3"
       :items [,(gnosis-test-dashboard--thema)]))
    (gnosis-themata--goto-id "thema-a")
    (let (callback)
      (cl-letf (((symbol-function 'gnosis-edit-thema)
                 (lambda (thema-id after-save)
                   (should (equal "thema-a" thema-id))
                   (setq callback after-save))))
        (gnosis-themata-edit))
      (funcall callback
               `(:thema_id "thema-a" :revision 4 :kind "basic"
                 :keimenon "Edited question" :hypothesis []
                 :answer ["Edited answer"] :tags ["new"]
                 :archived ,gnosis-core--json-false)))
    (let ((record (gethash "thema-a" gnosis-themata--records)))
      (should (= 4 (plist-get record :thema_revision)))
      (should (= 2 (plist-get record :state_revision))))
    (should (string-search "Edited question" (buffer-string)))
    (should (string-search "Edited answer" (buffer-string)))))

(provide 'gnosis-test-dashboard)
;;; gnosis-test-dashboard.el ends here
